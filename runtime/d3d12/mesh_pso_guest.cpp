// Create twelve distinct Xbox slot 86 mesh pipelines on 1, 2 or 4 threads and
// draw through the first TUXBOX_MESH_PSO_TEST_DRAWN of them (all by default).
// Each drawn pipeline writes its own value to 64 words between guard words;
// an undrawn one must leave its words at zero.
#include "mesh_fixture.h"
#include "frame_cli.h"
#include "mesh_ps.h"
#include "mesh_pso_ms0.h"
#include "mesh_pso_ms1.h"
#include "mesh_pso_ms2.h"
#include "mesh_pso_ms3.h"
#include "mesh_pso_ms4.h"
#include "mesh_pso_ms5.h"
#include "mesh_pso_ms6.h"
#include "mesh_pso_ms7.h"
#include "mesh_pso_ms8.h"
#include "mesh_pso_ms9.h"
#include "mesh_pso_ms10.h"
#include "mesh_pso_ms11.h"
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <future>

namespace {
using namespace mesh_fixture;
using Clock = std::chrono::steady_clock;
constexpr UINT cases = 12, pixels = 64, guard = 4, block = pixels + 2 * guard;
constexpr UINT sentinel = 0x98765432;
using Words = std::array<UINT, block * cases>;

template<std::size_t N>
D3D12_SHADER_BYTECODE shader(const unsigned char (&bytes)[N]) { return {bytes, N}; }

const std::array<D3D12_SHADER_BYTECODE, cases> programs{
    shader(mesh_pso_ms0), shader(mesh_pso_ms1), shader(mesh_pso_ms2), shader(mesh_pso_ms3),
    shader(mesh_pso_ms4), shader(mesh_pso_ms5), shader(mesh_pso_ms6), shader(mesh_pso_ms7),
    shader(mesh_pso_ms8), shader(mesh_pso_ms9), shader(mesh_pso_ms10), shader(mesh_pso_ms11)
};

long long nanos(Clock::duration duration) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(duration).count();
}

UINT jobs_from_environment() {
    wchar_t value[2]{};
    if (GetEnvironmentVariableW(L"TUXBOX_MESH_PSO_TEST_JOBS", value, 2) != 1 ||
        (value[0] != L'1' && value[0] != L'2' && value[0] != L'4')) {
        throw std::runtime_error("set TUXBOX_MESH_PSO_TEST_JOBS to 1, 2 or 4");
    }
    return UINT(value[0] - L'0');
}

UINT drawn_from_environment() {
    wchar_t value[3]{};
    const DWORD length = GetEnvironmentVariableW(L"TUXBOX_MESH_PSO_TEST_DRAWN", value, 3);
    if (length == 0) return cases;
    const UINT drawn = length == 1 ? UINT(value[0] - L'0') : length == 2 ? UINT(10 * (value[0] - L'0') + value[1] - L'0') : 0;
    if (drawn < 1 || drawn > cases) throw std::runtime_error("set TUXBOX_MESH_PSO_TEST_DRAWN to 1 through 12");
    return drawn;
}

UINT contribution(UINT variant, UINT group) {
    UINT value = (1u + 2u * group) ^ (0x9e3779b9u * (variant + 1u));
    for (UINT round = 0; round < 48; ++round) {
        value ^= value >> (5u + (round + variant) % 11u);
        value = value * 1664525u + 1013904223u + round;
    }
    return value;
}

struct Sample {
    long long start_ns{};
    long long call_ns{};
    HRESULT status{};
};

std::string run(const wchar_t* adapter, bool) {
    const UINT jobs = jobs_from_environment();
    const UINT drawn = drawn_from_environment();
    Context ctx(adapter, cases);
    auto* device = ctx.device.Get();
    auto* list = ctx.list.Get();
    D3D12_FEATURE_DATA_D3D12_OPTIONS7 caps{};
    check(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7, &caps, sizeof(caps)), "mesh options");
    if (caps.MeshShaderTier != D3D12_MESH_SHADER_TIER_1)
        throw std::runtime_error("selected adapter lacks mesh shader tier 1");
    D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};
    D3D12_ROOT_PARAMETER param{};
    param.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    param.DescriptorTable = {1, &range};
    D3D12_ROOT_SIGNATURE_DESC desc{1, &param, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    auto root = ctx.root(desc);

    std::array<MeshPipelineStream, cases> streams;
    for (UINT i = 0; i < cases; ++i) {
        streams[i] = stream(root.Get(), programs[i], shader(mesh_ps));
        streams[i].topology.payload = D3D12_PRIMITIVE_TOPOLOGY_TYPE_POINT;
    }
    std::array<ComPtr<ID3D12PipelineState>, cases> pipelines;
    std::array<Sample, cases> samples;
    std::promise<Clock::time_point> release;
    const auto start = release.get_future().share();
    std::array<std::future<void>, 4> workers;
    try {
        for (UINT worker = 0; worker < jobs; ++worker) {
            workers[worker] = std::async(std::launch::async, [&, worker] {
                const auto begin = start.get();
                for (UINT i = worker; i < cases; i += jobs) {
                    const auto call = Clock::now();
                    const HRESULT status = ctx.create_pipeline(streams[i], pipelines[i]);
                    samples[i] = {nanos(call - begin), nanos(Clock::now() - call), status};
                }
            });
        }
    } catch (...) {
        release.set_value(Clock::now());
        throw;
    }
    const auto begin = Clock::now();
    release.set_value(begin);
    for (UINT worker = 0; worker < jobs; ++worker) workers[worker].get();
    const auto total_ns = nanos(Clock::now() - begin);

    for (UINT i = 0; i < cases; ++i) {
        check(samples[i].status, "create benchmark mesh pipeline");
        if (!pipelines[i]) throw std::runtime_error("empty mesh pipeline");
    }

    Words expected{};
    expected.fill(sentinel);
    for (UINT i = 0; i < cases; ++i)
        std::fill_n(expected.begin() + i * block + guard, pixels, 0);
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(Words), D3D12_RESOURCE_STATE_GENERIC_READ);
    upload(seed.Get(), expected.data(), sizeof(Words));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, sizeof(Words), D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, sizeof(Words), D3D12_RESOURCE_STATE_COPY_DEST);
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, sizeof(Words));
    transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ctx.bind(root.Get(), 8, 8);
    for (UINT i = 0; i < drawn; ++i) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.Format = DXGI_FORMAT_R32_TYPELESS;
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Buffer.FirstElement = i * block + guard;
        view.Buffer.NumElements = pixels;
        view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view, ctx.cpu(i));
        list->SetPipelineState(pipelines[i].Get());
        ctx.table(0, i);
        ctx.dispatch(2, 1, 1);
        std::fill_n(expected.begin() + i * block + guard, pixels, contribution(i, 0) + contribution(i, 1));
    }
    transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, sizeof(Words));
    ctx.submit();

    D3D12_RANGE read{0, sizeof(Words)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map benchmark mesh pixels");
    Words observed;
    std::memcpy(observed.data(), mapped, sizeof(Words));
    readback->Unmap(0, &empty);
    for (size_t i = 0; i < expected.size(); ++i) {
        if (expected[i] != observed[i])
            throw std::runtime_error("word " + std::to_string(i) + " expected " + std::to_string(expected[i]) +
                                     " observed " + std::to_string(observed[i]));
    }
    char receipt[192];
    std::snprintf(receipt, sizeof(receipt),
                  "d3d12-mesh-pso PASS pci=%04x:%04x jobs=%u pipelines=%u drawn=%u pixels=%u/%u guards=%u total_ns=%lld",
                  ctx.adapter.VendorId, ctx.adapter.DeviceId, jobs, cases, drawn, cases * pixels, cases * pixels,
                  cases * guard * 2, total_ns);
    return receipt;
}
}

int wmain(int argc, wchar_t** argv) { return tuxbox_test::frame_main(argc, argv, "d3d12-mesh-pso", false, run); }
