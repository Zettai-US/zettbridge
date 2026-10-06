// SPDX-License-Identifier: MIT
// A pinned ggml buffer adapter; all tensor data remains in the lease mapping.
#include "kv_buffer.h"
#include "ggml-alloc.h"
#include "ggml-backend-impl.h"
#include "ggml-backend.h"
#include "zettbridge_uapi.h"
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <memory>
#include <stdexcept>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <thread>
#include <unistd.h>
#include <vector>

struct Allocation {
    bool remote, initial_clear = true;
    int fd = -1;
    uint64_t epoch = 0, handle = 0;
    uint8_t *base = nullptr;
    size_t bytes = 0;
    std::vector<ggml_tensor *> tensors;
    ~Allocation();
};
static bool enabled = false, use_remote = false;
static std::vector<Allocation *> allocations;
static uint64_t graph_refs = 0;
static bool release_failed = false;

static void ensure(bool ok, const char *message) {
    if (!ok)
        throw std::runtime_error(message);
}
static bool contains(const Allocation *a, const void *ptr, size_t bytes) {
    auto start = reinterpret_cast<uintptr_t>(a->base);
    auto address = reinterpret_cast<uintptr_t>(ptr);
    return address >= start && address - start <= a->bytes && bytes <= a->bytes - (address - start);
}
static bool isRemote(const void *ptr, size_t bytes) {
    for (const auto *a : allocations)
        if (a->remote && contains(a, ptr, bytes))
            return true;
    return false;
}
extern "C" void *__real_memcpy(void *, const void *, size_t);
extern "C" void *__real_memset(void *, int, size_t);
extern "C" void *__real_memmove(void *, const void *, size_t);
extern "C" void *__wrap_memcpy(void *dst, const void *src, size_t bytes) {
    if (!isRemote(dst, bytes) && !isRemote(src, bytes))
        return __real_memcpy(dst, src, bytes);
    auto *to = static_cast<volatile uint8_t *>(dst);
    auto *from = static_cast<const volatile uint8_t *>(src);
    size_t i = 0;
    // Both allocations are aligned, but tensor offsets need not be aligned.
    if ((reinterpret_cast<uintptr_t>(dst) | reinterpret_cast<uintptr_t>(src)) % 8 == 0) {
        for (; i + 8 <= bytes; i += 8)
            *reinterpret_cast<volatile uint64_t *>(to + i) = *reinterpret_cast<const volatile uint64_t *>(from + i);
    }
    for (; i < bytes; ++i)
        to[i] = from[i];
    return dst;
}
extern "C" void *__wrap_memset(void *dst, int value, size_t bytes) {
    if (!isRemote(dst, bytes))
        return __real_memset(dst, value, bytes);
    auto *to = static_cast<volatile uint8_t *>(dst);
    for (size_t i = 0; i < bytes; ++i)
        to[i] = static_cast<uint8_t>(value);
    return dst;
}
extern "C" void *__wrap_memmove(void *dst, const void *src, size_t bytes) {
    if (!isRemote(dst, bytes) && !isRemote(src, bytes))
        return __real_memmove(dst, src, bytes);
    if (reinterpret_cast<uintptr_t>(dst) <= reinterpret_cast<uintptr_t>(src))
        return __wrap_memcpy(dst, src, bytes);
    auto *to = static_cast<volatile uint8_t *>(dst);
    auto *from = static_cast<const volatile uint8_t *>(src);
    for (size_t i = bytes; i > 0; --i)
        to[i - 1] = from[i - 1];
    return dst;
}
Allocation::~Allocation() {
    allocations.erase(std::remove(allocations.begin(), allocations.end(), this), allocations.end());
    if (base)
        munmap(base, bytes);
    bool released = !remote || !handle;
    if (remote && handle) {
        zb_request r{};
        r.struct_size = sizeof(r);
        r.request_id = 2;
        r.export_epoch = epoch;
        r.object_handle = handle;
        if (ioctl(fd, ZB_IOC_RELEASE, &r) == 0) {
            for (unsigned i = 0; i < 500; ++i) {
                zb_request q{};
                q.struct_size = sizeof(q);
                q.request_id = 2;
                if (ioctl(fd, ZB_IOC_QUERY_TXN, &q))
                    break;
                if (!q.status) {
                    released = true;
                    break;
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(10));
            }
        }
        std::printf("{\"event\":\"KV_RELEASED\",\"handle\":%llu,\"bytes\":%zu,\"completed\":%s}\n",
                    static_cast<unsigned long long>(handle), bytes, released ? "true" : "false");
        std::fflush(stdout);
    }
    if (fd >= 0)
        close(fd);
    if (!released)
        release_failed = true;
}
static void freeBuffer(ggml_backend_buffer_t buffer) { delete static_cast<Allocation *>(buffer->context); }
static void *base(ggml_backend_buffer_t buffer) { return static_cast<Allocation *>(buffer->context)->base; }
static void clear(ggml_backend_buffer_t buffer, uint8_t value) {
    auto *a = static_cast<Allocation *>(buffer->context);
    if (a->initial_clear && value == 0) {
        a->initial_clear = false;
        return; // anonymous mmap / Rust ALLOC both prove a fresh zeroed range
    }
    a->initial_clear = false;
    __wrap_memset(a->base, value, a->bytes);
}
static void set(ggml_backend_buffer_t, ggml_tensor *tensor, const void *src, size_t offset, size_t bytes) {
    __wrap_memcpy(static_cast<uint8_t *>(tensor->data) + offset, src, bytes);
}
static void get(ggml_backend_buffer_t, const ggml_tensor *tensor, void *dst, size_t offset, size_t bytes) {
    __wrap_memcpy(dst, static_cast<const uint8_t *>(tensor->data) + offset, bytes);
}
static void fill(ggml_backend_buffer_t, ggml_tensor *tensor, uint8_t value, size_t offset, size_t bytes) {
    __wrap_memset(static_cast<uint8_t *>(tensor->data) + offset, value, bytes);
}
extern "C" ggml_backend_buffer_t __real_ggml_backend_alloc_ctx_tensors_from_buft(ggml_context *,
                                                                                 ggml_backend_buffer_type_t);
extern "C" ggml_backend_buffer_t __wrap_ggml_backend_alloc_ctx_tensors_from_buft(ggml_context *ctx,
                                                                                 ggml_backend_buffer_type_t buft) {
    if (!enabled)
        return __real_ggml_backend_alloc_ctx_tensors_from_buft(ctx, buft);
    unsigned count = 0;
    for (auto *t = ggml_get_first_tensor(ctx); t; t = ggml_get_next_tensor(ctx, t)) {
        if (!t->view_src && !t->data && std::strncmp(t->name, "cache_", 6) == 0)
            ++count;
    }
    if (!count)
        return __real_ggml_backend_alloc_ctx_tensors_from_buft(ctx, buft);
    auto a = std::make_unique<Allocation>();
    a->remote = use_remote;
    a->bytes =
        (ggml_backend_alloc_ctx_tensors_from_buft_size(ctx, ggml_backend_cpu_buffer_type()) + 4095) & ~size_t(4095);
    ensure(a->bytes > 0 && a->bytes <= (128ULL << 20), "unexpected KV allocation size");
    if (a->remote) {
        a->fd = open("/dev/rpu_consumer0", O_RDWR | O_CLOEXEC);
        ensure(a->fd >= 0, "open consumer KV device");
        zb_stats stats{};
        stats.struct_size = sizeof(stats);
        ensure(ioctl(a->fd, ZB_IOC_STATS, &stats) == 0 && stats.provider_state == 3, "KV pool inactive");
        a->epoch = stats.epoch;
        zb_request r{};
        r.struct_size = sizeof(r);
        r.request_id = 1;
        r.export_epoch = a->epoch;
        r.bytes = a->bytes;
        r.alignment = 4096;
        ensure(ioctl(a->fd, ZB_IOC_ALLOC, &r) == 0, "KV lease allocation failed");
        a->handle = r.object_handle;
        a->base = static_cast<uint8_t *>(
            mmap(nullptr, a->bytes, PROT_READ | PROT_WRITE, MAP_SHARED, a->fd, static_cast<off_t>(a->handle << 32)));
    } else {
        a->base =
            static_cast<uint8_t *>(mmap(nullptr, a->bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    }
    if (a->base == MAP_FAILED) {
        a->base = nullptr;
        throw std::runtime_error("KV mmap failed");
    }
    ggml_backend_buffer_i iface{};
    iface.free_buffer = freeBuffer;
    iface.get_base = base;
    iface.memset_tensor = fill;
    iface.set_tensor = set;
    iface.get_tensor = get;
    iface.clear = clear;
    ggml_backend_buffer_t buffer = ggml_backend_buffer_init(ggml_backend_cpu_buffer_type(), iface, a.get(), a->bytes);
    auto cleanup = [](ggml_backend_buffer *b) {
        b->context = nullptr; // Allocation is still owned by a on error.
        ggml_backend_buffer_free(b);
    };
    std::unique_ptr<ggml_backend_buffer, decltype(cleanup)> guard(buffer, cleanup);
    auto allocator = ggml_tallocr_new(buffer);
    for (auto *t = ggml_get_first_tensor(ctx); t; t = ggml_get_next_tensor(ctx, t)) {
        if (t->view_src) {
            ensure(ggml_backend_view_init(t) == GGML_STATUS_SUCCESS, "KV view allocation");
        } else {
            ensure(std::strncmp(t->name, "cache_", 6) == 0 && !t->data, "mixed KV/weight context rejected");
            ensure(ggml_tallocr_alloc(&allocator, t) == GGML_STATUS_SUCCESS, "KV tensor allocation");
            a->tensors.push_back(t);
        }
    }
    std::printf(
        "{\"event\":\"KV_MAPPED\",\"remote\":%s,\"bytes\":%zu,\"tensors\":%u,\"handle\":%llu,\"address\":\"%p\"}\n",
        a->remote ? "true" : "false", a->bytes, count, static_cast<unsigned long long>(a->handle), a->base);
    std::fflush(stdout);
    allocations.push_back(a.get());
    a.release();
    return guard.release();
}
void zbKvEnable(bool remote) {
    ensure(!enabled && allocations.empty(), "KV adapter is already active");
    enabled = true;
    use_remote = remote;
    graph_refs = 0;
    release_failed = false;
    zbAttentionReset();
}
bool zbKvActive() { return enabled; }
void zbKvDisable() {
    ensure(allocations.empty(), "KV buffer still live after context destruction");
    ensure(!release_failed, "KV lease release did not complete");
    enabled = false;
}
bool zbKvEval(ggml_tensor *t, bool ask, void *) {
    if (!ask || !enabled)
        return false;
    for (auto *src : t->src) {
        if (!src)
            continue;
        auto *root = src;
        while (root->view_src)
            root = root->view_src;
        if (std::strncmp(root->name, "cache_", 6))
            continue;
        bool found = false;
        for (const auto *a : allocations) {
            if (contains(a, src->data, ggml_nbytes(src)))
                found = true;
        }
        ensure(found, "graph KV tensor escaped its lease");
        ++graph_refs;
    }
    return false; // inspect addresses before execution; no tensor copy/readback
}
void zbKvVerify(unsigned round) {
    ensure(!allocations.empty(), "KV allocation hook was not invoked");
    size_t count = 0, bytes = 0;
    for (const auto *a : allocations) {
        for (const auto *t : a->tensors) {
            ensure(contains(a, t->data, ggml_nbytes(t)), "KV root outside lease");
            ensure(t->buffer && t->buffer->context == a, "KV backing changed during inference");
            ++count;
        }
        bytes += a->bytes;
    }
    ensure(graph_refs > 0, "inference did not reference the mapped KV tensors");
    std::printf("{\"event\":\"KV_DIRECT_VERIFIED\",\"round\":%u,\"remote\":%s,\"tensors\":%zu,\"bytes\":%zu,\"graph_"
                "refs\":%llu,\"attention_rows\":%llu}\n",
                round, use_remote ? "true" : "false", count, bytes, static_cast<unsigned long long>(graph_refs),
                static_cast<unsigned long long>(zbAttentionRows()));
    std::fflush(stdout);
}
