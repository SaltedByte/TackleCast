// Experimental Media Foundation -> DX12 buffer bridge. No pixel mappings.
#define NOMINMAX
#define _WIN32_WINNT 0x0A00
#define WINVER 0x0A00
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <mferror.h>
#include <d3d10.h>
#include <d3d11_4.h>
#include <d3d12.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <atomic>
#include <mutex>
#include <string>
#include <cmath>
#include <memory>
using Microsoft::WRL::ComPtr;
#define TRY_HR(x) do { HRESULT hr_ = (x); if (FAILED(hr_)) return hr_; } while (0)

class ReaderCallback final : public IMFSourceReaderCallback {
    std::atomic<ULONG> refs{1};
public:
    HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    std::mutex mutex;
    ComPtr<IMFSample> sample;
    HRESULT result = S_OK;
    DWORD flags = 0;
    ~ReaderCallback() { if (event) CloseHandle(event); }
    STDMETHODIMP QueryInterface(REFIID id, void** out) override {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (id == __uuidof(IUnknown) || id == __uuidof(IMFSourceReaderCallback)) {
            *out = static_cast<IMFSourceReaderCallback*>(this); AddRef(); return S_OK;
        }
        return E_NOINTERFACE;
    }
    STDMETHODIMP_(ULONG) AddRef() override { return ++refs; }
    STDMETHODIMP_(ULONG) Release() override { auto n = --refs; if (!n) delete this; return n; }
    STDMETHODIMP OnReadSample(HRESULT hr, DWORD, DWORD f, LONGLONG, IMFSample* value) override {
        std::lock_guard<std::mutex> lock(mutex);
        result = hr; flags = f; sample = value; SetEvent(event); return S_OK;
    }
    STDMETHODIMP OnEvent(DWORD, IMFMediaEvent*) override { return S_OK; }
    STDMETHODIMP OnFlush(DWORD) override { return S_OK; }
};
struct Session {
    ComPtr<IMFMediaSource> source;
    ComPtr<IMFSourceReader> reader;
    ComPtr<ReaderCallback> callback;
    ComPtr<ID3D11Device> device11;
    ComPtr<ID3D11DeviceContext4> context11;
    ComPtr<IMFDXGIDeviceManager> manager;
    ComPtr<ID3D12Device> device12;
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> commands;
    ComPtr<ID3D11Fence> producer;
    ComPtr<ID3D12Fence> producer12, completion;
    UINT64 value = 0;
    UINT width = 0, height = 0;
    UINT matrix = 0, nominalRange = 0, rateN = 0, rateD = 0;
    HANDLE completionEvent = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    bool pending = false, com = false, mf = false;
    ~Session() {
        if (reader) reader->Flush(MF_SOURCE_READER_ALL_STREAMS);
        if (source) source->Shutdown();
        reader.Reset(); source.Reset(); callback.Reset(); manager.Reset();
        if (completionEvent) CloseHandle(completionEvent);
        if (mf) MFShutdown();
        if (com) CoUninitialize();
    }
};
static HRESULT open_session(Session& s, void* rawDevice, const wchar_t* name,
                            UINT width, UINT height, UINT rateN, UINT rateD) {
    TRY_HR(CoInitializeEx(nullptr, COINIT_MULTITHREADED)); s.com = true;
    TRY_HR(MFStartup(MF_VERSION)); s.mf = true;
    s.width = width; s.height = height;
    if (!s.completionEvent) return E_OUTOFMEMORY;
    s.device12 = static_cast<ID3D12Device*>(rawDevice);
    const auto luid = s.device12->GetAdapterLuid();
    ComPtr<IDXGIFactory1> factory; TRY_HR(CreateDXGIFactory1(IID_PPV_ARGS(&factory)));
    ComPtr<IDXGIAdapter1> adapter;
    for (UINT i = 0; ; ++i) {
        ComPtr<IDXGIAdapter1> candidate;
        HRESULT hr = factory->EnumAdapters1(i, &candidate);
        if (hr == DXGI_ERROR_NOT_FOUND) break;
        TRY_HR(hr);
        DXGI_ADAPTER_DESC1 desc{}; TRY_HR(candidate->GetDesc1(&desc));
        if (desc.AdapterLuid.HighPart == luid.HighPart && desc.AdapterLuid.LowPart == luid.LowPart) { adapter = candidate; break; }
    }
    if (!adapter) return DXGI_ERROR_NOT_FOUND;
    TRY_HR(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        nullptr, 0, D3D11_SDK_VERSION, &s.device11, nullptr, nullptr));
    ComPtr<ID3D10Multithread> protection; TRY_HR(s.device11.As(&protection));
    protection->SetMultithreadProtected(TRUE);
    ComPtr<ID3D11DeviceContext> context; s.device11->GetImmediateContext(&context);
    TRY_HR(context.As(&s.context11));
    ComPtr<ID3D11Device5> device5; TRY_HR(s.device11.As(&device5));
    TRY_HR(device5->CreateFence(0, D3D11_FENCE_FLAG_SHARED, IID_PPV_ARGS(&s.producer)));
    HANDLE handle = nullptr;
    TRY_HR(s.producer->CreateSharedHandle(nullptr, GENERIC_ALL, nullptr, &handle));
    HRESULT hr = s.device12->OpenSharedHandle(handle, IID_PPV_ARGS(&s.producer12));
    CloseHandle(handle); TRY_HR(hr);
    D3D12_COMMAND_QUEUE_DESC q{}; q.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    TRY_HR(s.device12->CreateCommandQueue(&q, IID_PPV_ARGS(&s.queue)));
    TRY_HR(s.device12->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&s.completion)));
    TRY_HR(s.device12->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&s.allocator)));
    TRY_HR(s.device12->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT,
        s.allocator.Get(), nullptr, IID_PPV_ARGS(&s.commands)));
    TRY_HR(s.commands->Close());
    UINT token = 0; TRY_HR(MFCreateDXGIDeviceManager(&token, &s.manager));
    TRY_HR(s.manager->ResetDevice(s.device11.Get(), token));
    ComPtr<IMFAttributes> attrs; TRY_HR(MFCreateAttributes(&attrs, 1));
    TRY_HR(attrs->SetGUID(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID));
    IMFActivate** devices = nullptr; UINT count = 0;
    TRY_HR(MFEnumDeviceSources(attrs.Get(), &devices, &count));
    ComPtr<IMFActivate> chosen;
    UINT matches = 0;
    for (UINT i = 0; i < count; ++i) {
        wchar_t* label = nullptr; UINT length = 0;
        if (SUCCEEDED(devices[i]->GetAllocatedString(MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &label, &length))) {
            if (std::wstring(label) == name) { chosen = devices[i]; ++matches; }
            CoTaskMemFree(label);
        }
        devices[i]->Release();
    }
    CoTaskMemFree(devices);
    if (matches != 1) return MF_E_NOT_FOUND;
    TRY_HR(chosen->ActivateObject(IID_PPV_ARGS(&s.source)));
    s.callback.Attach(new ReaderCallback());
    if (!s.callback->event) return E_OUTOFMEMORY;
    attrs.Reset(); TRY_HR(MFCreateAttributes(&attrs, 4));
    TRY_HR(attrs->SetUnknown(MF_SOURCE_READER_ASYNC_CALLBACK, s.callback.Get()));
    TRY_HR(attrs->SetUnknown(MF_SOURCE_READER_D3D_MANAGER, s.manager.Get()));
    TRY_HR(attrs->SetUINT32(MF_READWRITE_DISABLE_CONVERTERS, TRUE));
    TRY_HR(attrs->SetUINT32(MF_LOW_LATENCY, TRUE));
    TRY_HR(MFCreateSourceReaderFromMediaSource(s.source.Get(), attrs.Get(), &s.reader));
    TRY_HR(s.reader->SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS, FALSE));
    TRY_HR(s.reader->SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM, TRUE));
    for (DWORD i = 0; ; ++i) {
        ComPtr<IMFMediaType> type;
        hr = s.reader->GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, i, &type);
        if (hr == MF_E_NO_MORE_TYPES) return hr;
        TRY_HR(hr);
        GUID subtype{}; UINT w=0,h=0,n=0,d=0;
        if (FAILED(type->GetGUID(MF_MT_SUBTYPE, &subtype)) || subtype != MFVideoFormat_NV12) continue;
        TRY_HR(MFGetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, &w, &h));
        TRY_HR(MFGetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, &n, &d));
        if (w == width && h == height && n && d && rateN && rateD &&
            std::abs(10000000.0*d/n - 10000000.0*rateD/rateN) <= 0.5)
        {
            TRY_HR(s.reader->SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, nullptr, type.Get()));
            ComPtr<IMFMediaType> current;
            TRY_HR(s.reader->GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, &current));
            // Missing/unsupported metadata stays unknown; the renderer labels
            // its defaults and still permits explicit user overrides.
            current->GetUINT32(MF_MT_YUV_MATRIX, &s.matrix);
            current->GetUINT32(MF_MT_VIDEO_NOMINAL_RANGE, &s.nominalRange);
            TRY_HR(MFGetAttributeRatio(current.Get(), MF_MT_FRAME_RATE, &s.rateN, &s.rateD));
            return S_OK;
        }
    }
}
struct GpuOutput { void* resource; UINT64 size, uvOffset; UINT yPitch, uvPitch; };
static HRESULT next_frame(Session& s, GpuOutput& out, ID3D12Resource* reuse = nullptr) {
    if (!s.pending) {
        TRY_HR(s.reader->ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0, nullptr, nullptr, nullptr, nullptr));
        s.pending = true;
    }
    if (WaitForSingleObject(s.callback->event, 100) == WAIT_TIMEOUT) return S_FALSE;
    s.pending = false;
    ComPtr<IMFSample> sample;
    {
        std::lock_guard<std::mutex> lock(s.callback->mutex);
        TRY_HR(s.callback->result);
        if (s.callback->flags & (MF_SOURCE_READERF_ERROR | MF_SOURCE_READERF_ENDOFSTREAM | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED)) return E_FAIL;
        sample = std::move(s.callback->sample);
    }
    if (!sample) return S_FALSE;
    // Keep one read outstanding while the current sample is copied. This
    // overlaps delivery with GPU work without building an application FIFO.
    // The local sample reference protects the current capture texture.
    TRY_HR(s.reader->ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0,
        nullptr, nullptr, nullptr, nullptr));
    s.pending = true;
    ComPtr<IMFMediaBuffer> buffer; TRY_HR(sample->GetBufferByIndex(0, &buffer));
    ComPtr<IMFDXGIBuffer> dxgi; TRY_HR(buffer.As(&dxgi));
    ComPtr<ID3D11Texture2D> texture; TRY_HR(dxgi->GetResource(IID_PPV_ARGS(&texture)));
    ComPtr<ID3D11Device> owner; texture->GetDevice(&owner);
    if (owner.Get() != s.device11.Get()) return E_NOTIMPL;
    UINT subresource = 0; TRY_HR(dxgi->GetSubresourceIndex(&subresource));
    D3D11_TEXTURE2D_DESC desc11{}; texture->GetDesc(&desc11);
    if (subresource || desc11.ArraySize != 1 || desc11.Format != DXGI_FORMAT_NV12 || desc11.Width != s.width || desc11.Height != s.height) return E_NOTIMPL;
    ComPtr<IDXGIResource1> shared; TRY_HR(texture.As(&shared));
    HANDLE handle = nullptr; TRY_HR(shared->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &handle));
    ComPtr<ID3D12Resource> source;
    HRESULT hr = s.device12->OpenSharedHandle(handle, IID_PPV_ARGS(&source)); CloseHandle(handle); TRY_HR(hr);
    const UINT64 value = ++s.value;
    TRY_HR(s.context11->Signal(s.producer.Get(), value)); s.context11->Flush();
    TRY_HR(s.queue->Wait(s.producer12.Get(), value));
    const auto desc = source->GetDesc();
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT layout[2]{}; UINT rows[2]{}; UINT64 rowBytes[2]{}, total=0;
    s.device12->GetCopyableFootprints(&desc, 0, 2, 0, layout, rows, rowBytes, &total);
    D3D12_HEAP_PROPERTIES heap{}; heap.Type = D3D12_HEAP_TYPE_DEFAULT; heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC bd{}; bd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER; bd.Width = total;
    bd.Height = bd.DepthOrArraySize = bd.MipLevels = 1; bd.SampleDesc.Count = 1; bd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> destination;
    if (reuse) {
        if (reuse->GetDesc().Dimension != D3D12_RESOURCE_DIMENSION_BUFFER || reuse->GetDesc().Width < total) return E_INVALIDARG;
        destination = reuse;
    } else {
        TRY_HR(s.device12->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &bd, D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&destination)));
    }
    // The previous call waited for completion before returning, so these
    // command objects can be reset safely. Never reset an in-flight allocator.
    TRY_HR(s.allocator->Reset());
    TRY_HR(s.commands->Reset(s.allocator.Get(), nullptr));
    auto& list = s.commands;
    D3D12_RESOURCE_BARRIER barrier{}; barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    if (reuse) {
        // Buffer resources decay to COMMON after the completed wgpu submission.
        // The caller must hold an exclusive pool lease before requesting reuse.
        barrier.Transition.pResource = destination.Get();
        barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_COMMON;
        barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_DEST;
        list->ResourceBarrier(1, &barrier);
    }
    barrier.Transition.pResource = source.Get(); barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_COMMON; barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
    list->ResourceBarrier(1, &barrier);
    for (UINT p=0;p<2;++p) {
        D3D12_TEXTURE_COPY_LOCATION src{},dst{};
        src.pResource=source.Get(); src.Type=D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX; src.SubresourceIndex=p;
        dst.pResource=destination.Get(); dst.Type=D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT; dst.PlacedFootprint=layout[p];
        list->CopyTextureRegion(&dst,0,0,0,&src,nullptr);
    }
    std::swap(barrier.Transition.StateBefore,barrier.Transition.StateAfter); list->ResourceBarrier(1,&barrier);
    barrier.Transition.pResource=destination.Get(); barrier.Transition.StateBefore=D3D12_RESOURCE_STATE_COPY_DEST;
    barrier.Transition.StateAfter=D3D12_RESOURCE_STATE_COMMON; list->ResourceBarrier(1,&barrier);
    TRY_HR(list->Close()); ID3D12CommandList* lists[]={list.Get()}; s.queue->ExecuteCommandLists(1,lists);
    TRY_HR(s.queue->Signal(s.completion.Get(),value));
    TRY_HR(s.completion->SetEventOnCompletion(value,s.completionEvent));
    if (WaitForSingleObject(s.completionEvent,5000)!=WAIT_OBJECT_0) return DXGI_ERROR_DEVICE_REMOVED;
    if (s.completion->GetCompletedValue()!=value) return DXGI_ERROR_DEVICE_REMOVED;
    TRY_HR(s.device12->GetDeviceRemovedReason());
    out={destination.Detach(),total,layout[1].Offset,layout[0].Footprint.RowPitch,layout[1].Footprint.RowPitch};
    return S_OK;
}
extern "C" __declspec(dllexport) HRESULT tc_mf_open(void* device,const wchar_t* name,UINT w,UINT h,UINT n,UINT d,void** out) {
    try { auto s = std::make_unique<Session>(); HRESULT hr=open_session(*s,device,name,w,h,n,d);
        if (FAILED(hr)) return hr;
        *out=s.release();
        return S_OK;
    } catch (...) { return E_FAIL; }
}
extern "C" __declspec(dllexport) HRESULT tc_mf_next(void* session,GpuOutput* out) {
    try { return next_frame(*static_cast<Session*>(session),*out); } catch (...) { return E_FAIL; }
}
extern "C" __declspec(dllexport) void tc_mf_close(void* session) { delete static_cast<Session*>(session); }

extern "C" __declspec(dllexport) UINT tc_mf_abi_version() { return 2; }
extern "C" __declspec(dllexport) HRESULT tc_mf_metadata(void* session, UINT* matrix, UINT* range, UINT* n, UINT* d) {
    if (!session || !matrix || !range || !n || !d) return E_POINTER;
    const auto& s = *static_cast<Session*>(session);
    *matrix = s.matrix; *range = s.nominalRange; *n = s.rateN; *d = s.rateD;
    return S_OK;
}
extern "C" __declspec(dllexport) HRESULT tc_mf_next_reuse(void* session, void* resource, GpuOutput* out) {
    try {
        if (!session || !resource || !out) return E_POINTER;
        return next_frame(*static_cast<Session*>(session), *out, static_cast<ID3D12Resource*>(resource));
    } catch (...) { return E_FAIL; }
}
extern "C" __declspec(dllexport) HRESULT tc_mf_allocate(void* session, GpuOutput* out) {
    try {
        if (!session || !out) return E_POINTER;
        auto& s = *static_cast<Session*>(session);
        D3D12_RESOURCE_DESC desc{};
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        desc.Width = s.width; desc.Height = s.height;
        desc.DepthOrArraySize = desc.MipLevels = 1;
        desc.Format = DXGI_FORMAT_NV12; desc.SampleDesc.Count = 1;
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT layout[2]{};
        UINT rows[2]{}; UINT64 rowBytes[2]{}, total = 0;
        s.device12->GetCopyableFootprints(&desc, 0, 2, 0, layout, rows, rowBytes, &total);
        D3D12_HEAP_PROPERTIES heap{}; heap.Type = D3D12_HEAP_TYPE_DEFAULT;
        heap.CreationNodeMask = heap.VisibleNodeMask = 1;
        desc = {}; desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER; desc.Width = total;
        desc.Height = desc.DepthOrArraySize = desc.MipLevels = 1;
        desc.SampleDesc.Count = 1; desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
        ComPtr<ID3D12Resource> resource;
        TRY_HR(s.device12->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
            D3D12_RESOURCE_STATE_COMMON, nullptr, IID_PPV_ARGS(&resource)));
        *out = {resource.Detach(), total, layout[1].Offset,
            layout[0].Footprint.RowPitch, layout[1].Footprint.RowPitch};
        return S_OK;
    } catch (...) { return E_FAIL; }
}
