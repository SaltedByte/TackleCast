// Standalone native-NV12 capture diagnostic. No pixel mapping or conversion.
#define NOMINMAX
#define _WIN32_WINNT 0x0A00
#define WINVER 0x0A00
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <mferror.h>
#include <d3d11.h>
#include <d3d11_4.h>
#include <d3d10.h>
#include <d3d12.h>
#include <dxgi1_2.h>
#include <dxgi.h>
#include <wrl/client.h>
#include <iostream>
#include <stdexcept>
#include <string>
#include <cmath>
#include <cstring>
using Microsoft::WRL::ComPtr;
static void check(HRESULT hr, const char* op) {
    if (FAILED(hr)) {
        std::cout << op << ": HRESULT=0x" << std::hex << hr << std::dec << std::endl;
        throw std::runtime_error(op);
    }
}
// Correctness diagnostic: all CPU mappings below verify GPU output only.
// A viewer backend must omit these readbacks and pool its consumer resources.
static void verify_handoff(ID3D11Texture2D* texture, UINT subresource,
                           ID3D11Device* owner, ID3D12Resource* imported,
                           ID3D12Device* device12) {
    ComPtr<ID3D11Device5> device5;
    check(owner->QueryInterface(IID_PPV_ARGS(&device5)), "Get fence-capable D3D11 device");
    ComPtr<ID3D11DeviceContext> context;
    owner->GetImmediateContext(&context);
    ComPtr<ID3D11DeviceContext4> context4;
    check(context.As(&context4), "Get fence-capable context");
    ComPtr<ID3D11Fence> producer;
    check(device5->CreateFence(0, D3D11_FENCE_FLAG_SHARED, IID_PPV_ARGS(&producer)), "Create producer fence");
    HANDLE handle = nullptr;
    check(producer->CreateSharedHandle(nullptr, GENERIC_ALL, nullptr, &handle), "Share producer fence");
    ComPtr<ID3D12Fence> producer12;
    const auto opened = device12->OpenSharedHandle(handle, IID_PPV_ARGS(&producer12));
    CloseHandle(handle);
    check(opened, "Import producer fence");
    check(context4->Signal(producer.Get(), 1), "Signal D3D11 producer fence");
    context->Flush();
    D3D12_COMMAND_QUEUE_DESC queueDesc{};
    queueDesc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ComPtr<ID3D12CommandQueue> queue;
    check(device12->CreateCommandQueue(&queueDesc, IID_PPV_ARGS(&queue)), "Create consumer queue");
    check(queue->Wait(producer12.Get(), 1), "GPU wait for D3D11 producer");
    ComPtr<ID3D12CommandAllocator> allocator;
    check(device12->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&allocator)), "Create allocator");
    ComPtr<ID3D12GraphicsCommandList> list;
    check(device12->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator.Get(), nullptr,
        IID_PPV_ARGS(&list)), "Create consumer commands");
    const auto desc = imported->GetDesc();
    if (desc.DepthOrArraySize != 1 || subresource != 0) throw std::runtime_error("Probe requires non-array capture texture");
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT layout[2]{};
    UINT rows[2]{}; UINT64 rowBytes[2]{}, total = 0;
    device12->GetCopyableFootprints(&desc, 0, 2, 0, layout, rows, rowBytes, &total);
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_READBACK;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC bufferDesc{};
    bufferDesc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    bufferDesc.Width = total;
    bufferDesc.Height = bufferDesc.DepthOrArraySize = bufferDesc.MipLevels = 1;
    bufferDesc.SampleDesc.Count = 1;
    bufferDesc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> readback;
    check(device12->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &bufferDesc,
        D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&readback)), "Create diagnostic readback");
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = imported;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_COMMON;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
    list->ResourceBarrier(1, &barrier);
    for (UINT p = 0; p < 2; ++p) {
        D3D12_TEXTURE_COPY_LOCATION src{}, dst{};
        src.pResource = imported; src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        src.SubresourceIndex = p;
        dst.pResource = readback.Get(); dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        dst.PlacedFootprint = layout[p];
        list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    }
    std::swap(barrier.Transition.StateBefore, barrier.Transition.StateAfter);
    list->ResourceBarrier(1, &barrier);
    check(list->Close(), "Close consumer commands");
    ID3D12CommandList* lists[] = {list.Get()}; queue->ExecuteCommandLists(1, lists);
    ComPtr<ID3D12Fence> completion;
    check(device12->CreateFence(0, D3D12_FENCE_FLAG_SHARED, IID_PPV_ARGS(&completion)), "Create consumer fence");
    check(queue->Signal(completion.Get(), 1), "Signal consumer completion");
    // Retain the IMFSample in the caller until this consumer is finished.
    const auto deadline = GetTickCount64() + 5000;
    while (completion->GetCompletedValue() < 1 && GetTickCount64() < deadline) Sleep(1);
    if (completion->GetCompletedValue() != 1) throw std::runtime_error("GPU consumer timed out or device removed");
    check(device12->GetDeviceRemovedReason(), "DX12 health");
    // Independent DX11 staging read provides a reference for both NV12 planes.
    D3D11_TEXTURE2D_DESC stagingDesc{}; texture->GetDesc(&stagingDesc);
    stagingDesc.BindFlags = stagingDesc.MiscFlags = 0;
    stagingDesc.Usage = D3D11_USAGE_STAGING;
    stagingDesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ComPtr<ID3D11Texture2D> staging;
    check(owner->CreateTexture2D(&stagingDesc, nullptr, &staging), "Create reference staging");
    context->CopyResource(staging.Get(), texture);
    D3D11_MAPPED_SUBRESOURCE mapped11{};
    check(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped11), "Map reference");
    struct Unmap11 { ID3D11DeviceContext* c; ID3D11Texture2D* t; ~Unmap11() { c->Unmap(t, 0); } } unmap11{context.Get(), staging.Get()};
    void* mapped12 = nullptr;
    D3D12_RANGE range{0, static_cast<SIZE_T>(total)};
    check(readback->Map(0, &range, &mapped12), "Map consumer verification");
    bool matches = true;
    for (UINT p = 0; p < 2; ++p) {
        const UINT height = p ? 720 : 1440;
        const size_t offset11 = p ? static_cast<size_t>(mapped11.RowPitch) * stagingDesc.Height : 0;
        for (UINT y = 0; y < height; ++y) {
            const auto a = static_cast<const char*>(mapped11.pData) + offset11 + static_cast<size_t>(y) * mapped11.RowPitch;
            const auto b = static_cast<const char*>(mapped12) + layout[p].Offset + static_cast<size_t>(y) * layout[p].Footprint.RowPitch;
            if (std::memcmp(a, b, 2560)) matches = false;
        }
    }
    D3D12_RANGE written{0, 0}; readback->Unmap(0, &written);
    if (!matches) throw std::runtime_error("DX12 consumer pixels do not match DX11 reference");
}

int wmain(int argc, wchar_t** argv) {
    try {
        const bool capture = argc >= 2 && std::wstring(argv[1]) == L"--capture";
        if (argc > 3 || (argc > 1 && !capture)) throw std::runtime_error("Usage: mf_gpu_capture_probe.exe [--capture [frames]]");
        const UINT targetFrames = argc == 3 ? static_cast<UINT>(std::stoul(argv[2])) : 60;
        if (!targetFrames || targetFrames > 2400) throw std::runtime_error("Invalid frame count");
        check(CoInitializeEx(nullptr, COINIT_MULTITHREADED), "COM startup");
        struct ComCleanup { ~ComCleanup() { CoUninitialize(); } } comCleanup;
        check(MFStartup(MF_VERSION), "MF startup");
        struct MfCleanup { ~MfCleanup() { MFShutdown(); } } mfCleanup;
        ComPtr<IMFAttributes> enumeration;
        check(MFCreateAttributes(&enumeration, 1), "Create enumeration attributes");
        check(enumeration->SetGUID(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID), "Set video source type");
        IMFActivate** devices = nullptr;
        UINT count = 0;
        check(MFEnumDeviceSources(enumeration.Get(), &devices, &count), "Enumerate video sources");
        struct DeviceCleanup { IMFActivate** p; UINT n; ~DeviceCleanup() {
            for (UINT i = 0; i < n; ++i) p[i]->Release(); CoTaskMemFree(p);
        } } deviceCleanup{devices, count};
        ComPtr<IMFActivate> chosen;
        for (UINT i = 0; i < count; ++i) {
            WCHAR* name = nullptr; UINT length = 0;
            check(devices[i]->GetAllocatedString(MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &name, &length), "Read source name");
            const std::wstring label(name); CoTaskMemFree(name);
            std::wcout << L"Device: " << label << std::endl;
            if (label.find(L"4K X") != std::wstring::npos || label.find(L"4KX") != std::wstring::npos) {
                if (chosen) throw std::runtime_error("Multiple 4K X devices; selection is ambiguous");
                chosen = devices[i];
            }
        }
        if (!chosen) throw std::runtime_error("No unambiguous Elgato 4K X Media Foundation source found");
        if (!capture) {
            std::cout << "Enumeration only; no device activated. Use --capture after closing capture apps." << std::endl;
            return 0;
        }
        ComPtr<IDXGIFactory1> factory;
        check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "Create DXGI factory");
        ComPtr<IDXGIAdapter1> adapter;
        for (UINT i = 0; ; ++i) {
            ComPtr<IDXGIAdapter1> candidate;
            if (factory->EnumAdapters1(i, &candidate) == DXGI_ERROR_NOT_FOUND) break;
            DXGI_ADAPTER_DESC1 desc{};
            check(candidate->GetDesc1(&desc), "Get adapter description");
            if (std::wstring(desc.Description).find(L"9070") != std::wstring::npos) {
                adapter = candidate; std::wcout << L"Adapter: " << desc.Description << std::endl; break;
            }
        }
        if (!adapter) throw std::runtime_error("No 9070 DXGI adapter found");
        ComPtr<ID3D11Device> device;
        check(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            nullptr, 0, D3D11_SDK_VERSION, &device, nullptr, nullptr), "Create D3D11 device");
        // Media Foundation and the diagnostic consumer use this device from
        // different threads. Serialize immediate-context access inside D3D11.
        ComPtr<ID3D10Multithread> multithread;
        check(device.As(&multithread), "Get D3D11 multithread protection");
        multithread->SetMultithreadProtected(TRUE);
        ComPtr<ID3D12Device> device12;
        check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device12)), "Create DX12 consumer device");
        UINT token = 0;
        ComPtr<IMFDXGIDeviceManager> manager;
        check(MFCreateDXGIDeviceManager(&token, &manager), "Create DXGI manager");
        check(manager->ResetDevice(device.Get(), token), "Set DXGI device");
        ComPtr<IMFMediaSource> source;
        check(chosen->ActivateObject(IID_PPV_ARGS(&source)), "Activate 4K X");
        struct SourceCleanup { IMFMediaSource* p; ~SourceCleanup() { if (p) p->Shutdown(); } } sourceCleanup{source.Get()};
        ComPtr<IMFAttributes> attributes;
        check(MFCreateAttributes(&attributes, 3), "Create reader attributes");
        check(attributes->SetUnknown(MF_SOURCE_READER_D3D_MANAGER, manager.Get()), "Set reader D3D manager");
        check(attributes->SetUINT32(MF_READWRITE_DISABLE_CONVERTERS, TRUE), "Disable converters");
        check(attributes->SetUINT32(MF_LOW_LATENCY, TRUE), "Set low latency");
        ComPtr<IMFSourceReader> reader;
        check(MFCreateSourceReaderFromMediaSource(source.Get(), attributes.Get(), &reader), "Create source reader");
        check(reader->SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS, FALSE), "Deselect streams");
        check(reader->SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM, TRUE), "Select video");
        bool selected = false;
        for (DWORD i = 0; ; ++i) {
            ComPtr<IMFMediaType> type;
            const auto result = reader->GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, i, &type);
            if (result == MF_E_NO_MORE_TYPES) break;
            check(result, "Enumerate native mode");
            GUID subtype{}; UINT32 w = 0, h = 0, n = 0, d = 0;
            if (FAILED(type->GetGUID(MF_MT_SUBTYPE, &subtype)) || subtype != MFVideoFormat_NV12) continue;
            check(MFGetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, &w, &h), "Read size");
            check(MFGetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, &n, &d), "Read frame rate");
            if (w == 2560 && h == 1440) std::cout << "Native NV12: " << w << "x" << h << " " << n << "/" << d << std::endl;
            // Windows capture intervals have 100 ns precision: 120 Hz is
            // normally advertised as 10000000/83333, not exactly 120/1.
            const bool nominal120 = n && d && std::abs(10000000.0 * d / n - 10000000.0 / 120.0) <= 0.5;
            if (!selected && w == 2560 && h == 1440 && nominal120) {
                std::cout << "Selected advertised rate " << n << "/" << d << std::endl;
                check(reader->SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, nullptr, type.Get()), "Select native 1440p120 NV12");
                selected = true;
            }
        }
        if (!selected) throw std::runtime_error("Exact native 1440p120 NV12 unavailable; no lower-mode fallback");
        UINT gpu = 0, cpu = 0, frames = 0;
        for (UINT attempt = 0; frames < targetFrames && attempt < targetFrames * 5; ++attempt) {
            DWORD stream = 0, flags = 0; LONGLONG timestamp = 0;
            ComPtr<IMFSample> sample;
            check(reader->ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0, &stream, &flags, &timestamp, &sample), "Read frame");
            if (flags & (MF_SOURCE_READERF_ERROR | MF_SOURCE_READERF_ENDOFSTREAM | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED))
                throw std::runtime_error("Stream error, end, or unexpected mode change");
            if (!sample) continue;
            DWORD buffers = 0; check(sample->GetBufferCount(&buffers), "Read buffer count");
            if (!buffers) throw std::runtime_error("Empty sample");
            for (DWORD i = 0; i < buffers; ++i) {
                ComPtr<IMFMediaBuffer> buffer; check(sample->GetBufferByIndex(i, &buffer), "Get buffer");
                ComPtr<IMFDXGIBuffer> dxgi;
                if (SUCCEEDED(buffer.As(&dxgi))) {
                    ComPtr<ID3D11Texture2D> texture;
                    check(dxgi->GetResource(IID_PPV_ARGS(&texture)), "Get GPU texture");
                    D3D11_TEXTURE2D_DESC desc{}; texture->GetDesc(&desc);
                    ComPtr<ID3D11Device> owner;
                    texture->GetDevice(&owner);
                    ComPtr<IDXGIDevice> ownerDxgi;
                    check(owner.As(&ownerDxgi), "Get capture DXGI device");
                    ComPtr<IDXGIAdapter> ownerAdapter;
                    check(ownerDxgi->GetAdapter(&ownerAdapter), "Get capture adapter");
                    DXGI_ADAPTER_DESC ownerDesc{};
                    check(ownerAdapter->GetDesc(&ownerDesc), "Get capture adapter identity");
                    DXGI_ADAPTER_DESC1 selectedDesc{};
                    check(adapter->GetDesc1(&selectedDesc), "Get selected adapter identity");
                    if (ownerDesc.AdapterLuid.HighPart != selectedDesc.AdapterLuid.HighPart ||
                        ownerDesc.AdapterLuid.LowPart != selectedDesc.AdapterLuid.LowPart)
                        throw std::runtime_error("Capture texture is on a different adapter");
                    UINT subresource = 0;
                    check(dxgi->GetSubresourceIndex(&subresource), "Get texture subresource");
                    if (!gpu) std::cout << "GPU texture: " << desc.Width << "x" << desc.Height
                        << " format=" << desc.Format << " array=" << desc.ArraySize
                        << " subresource=" << subresource << " bindFlags=" << desc.BindFlags
                        << " miscFlags=" << desc.MiscFlags << " sameDevice=" << (owner.Get() == device.Get())
                        << " sameAdapter=true" << std::endl;
                    ComPtr<IDXGIResource1> shareable;
                    check(texture.As(&shareable), "Get shareable capture resource");
                    HANDLE handle = nullptr;
                    check(shareable->CreateSharedHandle(nullptr,
                        DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &handle), "Export capture texture handle");
                    ComPtr<ID3D12Resource> imported;
                    const auto importResult = device12->OpenSharedHandle(handle, IID_PPV_ARGS(&imported));
                    CloseHandle(handle);
                    check(importResult, "Import native capture texture into DX12");
                    const auto importedDesc = imported->GetDesc();
                    if (importedDesc.Format != DXGI_FORMAT_NV12 || importedDesc.Width != desc.Width || importedDesc.Height != desc.Height)
                        throw std::runtime_error("DX12 imported texture layout mismatch");
                    verify_handoff(texture.Get(), subresource, owner.Get(), imported.Get(), device12.Get());
                    if (desc.Format != DXGI_FORMAT_NV12 || desc.Width < 2560 || desc.Height < 1440)
                        throw std::runtime_error("Unexpected GPU resource layout");
                    ++gpu;
                } else ++cpu;
            }
            ++frames;
        }
        std::cout << "RESULT: frames=" << frames << " DXGI buffers=" << gpu << " CPU buffers=" << cpu << std::endl;
        if (gpu) std::cout << "PASS: all GPU textures imported, producer-fenced, consumed in DX12, and matched DX11 reference pixels." << std::endl;
        if (frames != targetFrames) throw std::runtime_error("Incomplete sample count");
        std::cout << (gpu && !cpu ? "CANDIDATE: native GPU delivery and DX12 copy verified; wgpu integration and driver-internal staging remain unverified."
            : "CPU-backed capture delivered; this reader configuration does not establish GPU-only capture.") << std::endl;
        // Stop delivery explicitly while the reader, device manager and both
        // graphics devices still exist; do not rely on reader destruction.
        check(reader->Flush(MF_SOURCE_READER_ALL_STREAMS), "Flush capture reader");
        check(source->Shutdown(), "Shutdown capture source");
        sourceCleanup.p = nullptr;
        reader.Reset();
        source.Reset();
        return 0;
    } catch (const std::exception& e) { std::cerr << "FAIL: " << e.what() << std::endl; return 1; }
}
