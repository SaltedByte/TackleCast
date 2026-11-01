// Standalone Windows diagnostic; deliberately independent of the app and FFmpeg.
#define NOMINMAX
#include <windows.h>
#include <d3d11.h>
#include <d3d12.h>
#include <dxgi.h>
#include <wrl/client.h>
#include <fstream>
#include <filesystem>
#include <iostream>
#include <iterator>
#include <vector>
#include <cstring>
#include <cwchar>
#include <string>
#include <stdexcept>
#include "core/Factory.h"
#include "core/D3D12AMF.h"
#include "components/VideoDecoderUVD.h"
#include "components/ComponentCaps.h"

using Microsoft::WRL::ComPtr;
using namespace amf;

static void check_hr(HRESULT hr, const char* operation) {
    if (FAILED(hr)) {
        std::cout << operation << ": HRESULT=0x" << std::hex << hr << std::dec << std::endl;
        throw std::runtime_error(operation);
    }
}

// Exercise the consumer handoff without mapping decoded pixels on the CPU.
// This is a native DX12 consumer, not yet a wgpu integration test.
static std::vector<std::vector<char>> consume_dx12(AMFSurface* surface, ID3D12Device* device) {
    if (surface->GetMemoryType() != AMF_MEMORY_DX12)
        throw std::runtime_error("Decoder did not return a DX12 surface");
    auto source = static_cast<ID3D12Resource*>(surface->GetPlaneAt(0)->GetNative());
    if (!source) throw std::runtime_error("Missing native DX12 resource");
    ComPtr<ID3D12Device> owner;
    check_hr(source->GetDevice(IID_PPV_ARGS(&owner)), "Get resource device");
    if (owner.Get() != device) throw std::runtime_error("Unexpected DX12 device");
    auto desc = source->GetDesc();
    std::cout << "DX12 texture: " << desc.Width << "x" << desc.Height
              << " format=" << desc.Format << " array=" << desc.DepthOrArraySize << std::endl;
    D3D12_RESOURCE_STATES state{};
    UINT size = sizeof(state);
    check_hr(source->GetPrivateData(AMFResourceStateGUID, &size, &state), "Get AMF resource state");
    ComPtr<ID3D12Fence> producerFence;
    size = sizeof(ID3D12Fence*);
    check_hr(source->GetPrivateData(AMFFenceGUID, &size,
                                  producerFence.GetAddressOf()), "Get AMF producer fence");
    if (!producerFence) throw std::runtime_error("Missing producer fence");
    UINT64 producerValue = 0;
    size = sizeof(producerValue);
    check_hr(producerFence->GetPrivateData(AMFFenceValueGUID, &size, &producerValue),
             "Get AMF producer fence value");
    D3D12_COMMAND_QUEUE_DESC queueDesc{};
    queueDesc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ComPtr<ID3D12CommandQueue> queue;
    check_hr(device->CreateCommandQueue(&queueDesc, IID_PPV_ARGS(&queue)), "Create consumer queue");
    check_hr(queue->Wait(producerFence.Get(), producerValue), "Queue wait on decoder");
    ComPtr<ID3D12CommandAllocator> allocator;
    check_hr(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,
                                           IID_PPV_ARGS(&allocator)), "Create allocator");
    ComPtr<ID3D12GraphicsCommandList> list;
    check_hr(device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator.Get(),
                                      nullptr, IID_PPV_ARGS(&list)), "Create consumer list");
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    desc.Flags = D3D12_RESOURCE_FLAG_NONE;
    ComPtr<ID3D12Resource> destination;
    check_hr(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
              D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&destination)),
              "Create GPU consumer texture");
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = source;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = state;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
    if (state != D3D12_RESOURCE_STATE_COPY_SOURCE) list->ResourceBarrier(1, &barrier);
    list->CopyResource(destination.Get(), source);
    std::swap(barrier.Transition.StateBefore, barrier.Transition.StateAfter);
    if (state != D3D12_RESOURCE_STATE_COPY_SOURCE) list->ResourceBarrier(1, &barrier);
    // Diagnostic readback of the CONSUMER texture checks the handoff's content.
    // This readback is verification, not part of the proposed rendering path.
    const UINT planes = surface->GetFormat() == AMF_SURFACE_NV12 ? 2 : 1;
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprints[2]{};
    UINT rows[2]{};
    UINT64 rowBytes[2]{}, total = 0;
    device->GetCopyableFootprints(&desc, 0, planes, 0, footprints, rows, rowBytes, &total);
    D3D12_RESOURCE_DESC bufferDesc{};
    bufferDesc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    bufferDesc.Width = total;
    bufferDesc.Height = bufferDesc.DepthOrArraySize = bufferDesc.MipLevels = 1;
    bufferDesc.SampleDesc.Count = 1;
    bufferDesc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    heap.Type = D3D12_HEAP_TYPE_READBACK;
    ComPtr<ID3D12Resource> readback;
    check_hr(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &bufferDesc,
        D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&readback)), "Create verification readback");
    barrier.Transition.pResource = destination.Get();
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_COPY_DEST;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
    list->ResourceBarrier(1, &barrier);
    for (UINT p = 0; p < planes; ++p) {
        D3D12_TEXTURE_COPY_LOCATION src{}, dst{};
        src.pResource = destination.Get();
        src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        src.SubresourceIndex = p;
        dst.pResource = readback.Get();
        dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        dst.PlacedFootprint = footprints[p];
        list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    }
    check_hr(list->Close(), "Close consumer list");
    ID3D12CommandList* lists[] = {list.Get()};
    queue->ExecuteCommandLists(1, lists);
    ComPtr<ID3D12Fence> completion;
    check_hr(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&completion)),
             "Create consumer fence");
    check_hr(queue->Signal(completion.Get(), 1), "Signal consumer completion");
    // Keep the AMF surface alive until the GPU has finished reading it.
    const auto deadline = GetTickCount64() + 5000;
    while (completion->GetCompletedValue() < 1 && GetTickCount64() < deadline) Sleep(1);
    if (completion->GetCompletedValue() != 1)
        throw std::runtime_error("Consumer completion timeout or device removed");
    check_hr(device->GetDeviceRemovedReason(), "DX12 device health");
    std::cout << "PASS: decoder fence -> GPU texture copy -> consumer fence"
              << std::endl;
    void* mapped = nullptr;
    D3D12_RANGE range{0, static_cast<SIZE_T>(total)};
    check_hr(readback->Map(0, &range, &mapped), "Map consumer verification pixels");
    std::vector<std::vector<char>> pixels(planes);
    for (UINT p = 0; p < planes; ++p) {
        auto plane = surface->GetPlaneAt(p);
        const auto bytes = plane->GetWidth() * plane->GetPixelSizeInBytes();
        pixels[p].resize(static_cast<size_t>(bytes) * plane->GetHeight());
        for (int y = 0; y < plane->GetHeight(); ++y)
            std::memcpy(pixels[p].data() + static_cast<size_t>(y) * bytes,
                static_cast<char*>(mapped) + footprints[p].Offset +
                static_cast<size_t>(y) * footprints[p].Footprint.RowPitch, bytes);
    }
    D3D12_RANGE written{0, 0};
    readback->Unmap(0, &written);
    return pixels;
}

static void check(AMF_RESULT result, const char* operation) {
    std::cout << operation << ": AMF_RESULT=" << result << std::endl;
    if (result != AMF_OK) throw std::runtime_error(operation);
}

static bool decode(AMFFactory* factory, AMFContext* context,
                   const std::vector<char>& jpeg, int width, int height,
                   AMF_SURFACE_FORMAT format, ID3D12Device* dx12 = nullptr, bool bridge = false,
                   int frames = 1) {
    AMFComponentPtr decoder;
    check(factory->CreateComponent(context, AMFVideoDecoderUVD_MJPEG, &decoder),
          "Create MJPEG component");
    struct Terminate { AMFComponent* p; ~Terminate() { p->Terminate(); } } cleanup{decoder};
    AMFCapsPtr caps;
    check(decoder->GetCaps(&caps), "GetCaps");
    const auto acceleration = caps->GetAccelerationType();
    std::cout << "Acceleration=" << acceleration
              << " (0=hardware, 1=GPU, 2=software, -1=unsupported)" << std::endl;
    if (acceleration != AMF_ACCEL_HARDWARE && acceleration != AMF_ACCEL_GPU)
        throw std::runtime_error("GPU acceleration unavailable for this context");
    if (frames > 1) {
        check(decoder->SetProperty(AMF_VIDEO_DECODER_REORDER_MODE,
            static_cast<amf_int64>(AMF_VIDEO_DECODER_MODE_LOW_LATENCY)), "Set low-latency reorder mode");
    }
    check(decoder->Init(format, width, height), "Init MJPEG");
    const auto started = GetTickCount64();
    for (int frame = 0; frame < frames; ++frame) {
    AMFBufferPtr input;
    check(context->AllocBuffer(AMF_MEMORY_HOST, jpeg.size(), &input), "AllocBuffer");
    std::memcpy(input->GetNative(), jpeg.data(), jpeg.size());
    input->SetPts(frame * (AMF_SECOND / 120));
    input->SetDuration(AMF_SECOND / 60);
    check(decoder->SubmitInput(input), "Submit JPEG");
    // Flush this single-frame stream so regular-mode buffering cannot hide output.
    bool draining = frames > 1;
    bool received = false;
    const ULONGLONG deadline = GetTickCount64() + 5000;
    while (GetTickCount64() < deadline) {
        if (!draining) {
            const auto result = decoder->Drain();
            if (result == AMF_OK) draining = true;
            else if (result != AMF_INPUT_FULL) check(result, "Drain");
        }
        AMFDataPtr output;
        const auto result = decoder->QueryOutput(&output);
        if (output) {
            AMFSurfacePtr surface(output);
            if (!surface) throw std::runtime_error("Output is not a surface");
            std::cout << "Decoded surface: format=" << surface->GetFormat()
                      << " memory=" << surface->GetMemoryType() << std::endl;
            std::vector<std::vector<char>> consumerPixels;
            if (dx12) {
                if (bridge && frame == 0) {
                    AMFContext2Ptr context2(context);
                    if (!context2) throw std::runtime_error("AMFContext2 unavailable");
                    check(context2->InitDX12(dx12), "Init DX12 after hardware decode");
                }
                if (surface->GetMemoryType() != AMF_MEMORY_DX12)
                    check(surface->Convert(AMF_MEMORY_DX12), "GPU transfer DX11 -> DX12");
                consumerPixels = consume_dx12(surface, dx12);
            }
            check(surface->Convert(AMF_MEMORY_HOST), "Readback");
            for (size_t p = 0; p < consumerPixels.size(); ++p) {
                auto hostPlane = surface->GetPlaneAt(static_cast<amf_int32>(p));
                const auto bytes = hostPlane->GetWidth() * hostPlane->GetPixelSizeInBytes();
                for (int y = 0; y < hostPlane->GetHeight(); ++y)
                    if (std::memcmp(consumerPixels[p].data() + static_cast<size_t>(y) * bytes,
                        static_cast<char*>(hostPlane->GetNative()) + static_cast<size_t>(y) * hostPlane->GetHPitch(), bytes))
                        {
                            std::cout << "Mismatch plane=" << p << " row=" << y
                                      << " width=" << hostPlane->GetWidth()
                                      << " pixelBytes=" << hostPlane->GetPixelSizeInBytes()
                                      << " offset=" << hostPlane->GetOffsetX() << "," << hostPlane->GetOffsetY()
                                      << std::endl;
                            throw std::runtime_error("DX12 consumer pixels differ from AMF readback");
                        }
            }
            if (!consumerPixels.empty()) std::cout << "PASS: all visible consumer planes match AMF readback byte-for-byte" << std::endl;
            auto plane = surface->GetPlaneAt(0);
            if (!plane || !plane->GetNative() || plane->GetWidth() != width ||
                plane->GetHeight() != height) {
                throw std::runtime_error("Invalid output plane or unexpected dimensions");
            }
            std::cout << "Output: " << plane->GetWidth() << "x" << plane->GetHeight()
                      << ", pitch=" << plane->GetHPitch() << std::endl;
            if (acceleration != AMF_ACCEL_HARDWARE && acceleration != AMF_ACCEL_GPU)
                throw std::runtime_error("Decode succeeded without GPU acceleration");
            std::cout << "PASS: GPU decode and host readback succeeded" << std::endl;
            received = true;
            break;
        }
        if (result == AMF_EOF) break;
        if (result != AMF_OK && result != AMF_REPEAT) check(result, "QueryOutput");
        Sleep(1);
    }
    if (!received) throw std::runtime_error("No decoded output within five seconds");
    }
    std::cout << "PASS: " << frames << " frames through one decoder; elapsed="
              << GetTickCount64() - started << "ms (includes allocations and diagnostic readbacks; not a playback benchmark)"
              << std::endl;
    return true;
}

int wmain(int argc, wchar_t** argv) {
    try {
        if (argc < 4 || argc > 6) throw std::runtime_error("Usage: amf_probe.exe image.jpg width height [dx11|dx12|bridge] [frames]");
        const bool useDx12 = argc >= 5 && std::wcscmp(argv[4], L"dx12") == 0;
        const bool bridge = argc >= 5 && std::wcscmp(argv[4], L"bridge") == 0;
        if (argc >= 5 && !useDx12 && !bridge && std::wcscmp(argv[4], L"dx11")) throw std::runtime_error("Unknown backend");
        const int frames = argc == 6 ? std::stoi(argv[5]) : 1;
        if (frames < 1 || frames > 10000) throw std::runtime_error("Invalid frame count");
        const int width = std::stoi(argv[2]), height = std::stoi(argv[3]);
        if (width <= 0 || height <= 0) throw std::runtime_error("Invalid dimensions");
        std::ifstream file(std::filesystem::path(argv[1]), std::ios::binary);
        if (!file) throw std::runtime_error("Cannot open JPEG");
        std::vector<char> jpeg((std::istreambuf_iterator<char>(file)), {});
        if (jpeg.empty()) throw std::runtime_error("Empty JPEG");
        ComPtr<IDXGIFactory1> dxgi;
        if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&dxgi))))
            throw std::runtime_error("CreateDXGIFactory1 failed");
        ComPtr<IDXGIAdapter1> adapter;
        for (UINT index = 0; ; ++index) {
            ComPtr<IDXGIAdapter1> candidate;
            const auto result = dxgi->EnumAdapters1(index, &candidate);
            if (result == DXGI_ERROR_NOT_FOUND) break;
            if (FAILED(result)) throw std::runtime_error("EnumAdapters1 failed");
            DXGI_ADAPTER_DESC1 desc{};
            if (FAILED(candidate->GetDesc1(&desc))) continue;
            if (desc.VendorId == 0x1002 && !(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) {
                // Prefer the user's 9070 over an AMD integrated GPU.
                if (!adapter || std::wcsstr(desc.Description, L"9070")) adapter = candidate;
                if (std::wcsstr(desc.Description, L"9070")) break;
            }
        }
        if (!adapter) throw std::runtime_error("No AMD DXGI adapter found");
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        std::wcout << L"Adapter: " << desc.Description << std::endl;
        ComPtr<ID3D11Device> device;
        ComPtr<ID3D12Device> device12;
        const auto hr = useDx12 ? D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0,
            IID_PPV_ARGS(&device12)) : D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
            &device, nullptr, nullptr);
        if (FAILED(hr)) {
            std::cout << "D3D11 HRESULT=0x" << std::hex << hr << std::dec << std::endl;
            throw std::runtime_error("D3D11CreateDevice failed");
        }
        if (bridge) check_hr(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0,
                                              IID_PPV_ARGS(&device12)), "Create bridge DX12 device");
        // Load only the installed driver runtime. Keep it loaded until process exit.
        const auto library = LoadLibraryExW(L"amfrt64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!library) throw std::runtime_error("Installed amfrt64.dll not found in System32");
        const auto init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(library, AMF_INIT_FUNCTION_NAME));
        const auto version = reinterpret_cast<AMFQueryVersion_Fn>(GetProcAddress(library, AMF_QUERY_VERSION_FUNCTION_NAME));
        if (!init || !version) throw std::runtime_error("Missing AMF entry points");
        amf_uint64 runtimeVersion = 0;
        check(version(&runtimeVersion), "AMFQueryVersion");
        std::cout << "AMF runtime version=0x" << std::hex << runtimeVersion << std::dec << std::endl;
        AMFFactory* factory = nullptr;
        check(init(AMF_FULL_VERSION, &factory), "AMFInit");
        bool passed = false;
        for (auto format : {AMF_SURFACE_NV12, AMF_SURFACE_YUY2}) {
        if (frames > 1 && format != AMF_SURFACE_YUY2) continue;
        // DX12 initialization changes capability selection; isolate each format.
        AMFContextPtr context;
        check(factory->CreateContext(&context), "CreateContext");
        struct Terminate { AMFContext* p; ~Terminate() { p->Terminate(); } } cleanup{context};
        if (useDx12) {
            AMFContext2Ptr context2(context);
            if (!context2) throw std::runtime_error("AMFContext2 unavailable");
            check(context2->InitDX12(device12.Get()), "InitDX12");
        }
        if (!useDx12) check(context->InitDX11(device.Get()), "InitDX11");
            std::cout << "Trying output " << (format == AMF_SURFACE_NV12 ? "NV12" : "YUY2") << std::endl;
            try { passed = decode(factory, context, jpeg, width, height, format, device12.Get(), bridge, frames) || passed; }
            catch (const std::exception& error) { std::cout << "FAIL: " << error.what() << std::endl; }
        }
        return passed ? 0 : 1;
    } catch (const std::exception& error) {
        std::cerr << "FAIL: " << error.what() << std::endl;
        return 1;
    }
}
