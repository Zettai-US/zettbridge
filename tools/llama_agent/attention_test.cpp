// SPDX-License-Identifier: MIT
// Compare every output byte with upstream FA, including masks and thread tails.
#include "ggml-backend.h"
#include "ggml-cpu.h"
#include "kv_buffer.h"
#include <cmath>
#include <cstring>
#include <iostream>
#include <stdexcept>
#include <vector>

static void require(bool ok, const char *message) {
    if (!ok)
        throw std::runtime_error(message);
}
static void check(int nq, int nk, int dk, int dv, int heads, int kv_heads, int threads, int masks, float bias) {
    ggml_init_params init{};
    init.mem_size = 1 << 20;
    init.no_alloc = true;
    auto *ctx = ggml_init(init);
    auto backend = ggml_backend_cpu_init();
    ggml_backend_cpu_set_n_threads(backend, threads);
    auto *q = ggml_new_tensor_4d(ctx, GGML_TYPE_F32, dk, nq, heads, 1);
    auto *k = ggml_new_tensor_4d(ctx, GGML_TYPE_F16, dk, nk, kv_heads, 1);
    auto *v = ggml_new_tensor_4d(ctx, GGML_TYPE_F16, dv, nk, kv_heads, 1);
    auto *mask = masks ? ggml_new_tensor_4d(ctx, GGML_TYPE_F16, nk, nq, masks, 1) : nullptr;
    auto *out = ggml_flash_attn_ext(ctx, q, k, v, mask, 1.0f / sqrtf(dk), bias, 0.0f);
    auto *graph = ggml_new_graph(ctx);
    ggml_build_forward_expand(graph, out);
    auto buffer = ggml_backend_alloc_ctx_tensors(ctx, backend);
    require(buffer, "allocate graph");
    for (auto *tensor : {q, k, v}) {
        for (int64_t i = 0; i < ggml_nelements(tensor); ++i) {
            float value = sinf(float(i * 37 % 103)) * 0.8f;
            if (tensor->type == GGML_TYPE_F32)
                static_cast<float *>(tensor->data)[i] = value;
            else
                static_cast<ggml_fp16_t *>(tensor->data)[i] = ggml_fp32_to_fp16(value);
        }
    }
    if (mask) {
        for (int h = 0; h < masks; ++h)
            for (int t = 0; t < nq; ++t)
                for (int cell = 0; cell < nk; ++cell) {
                    // The first query is entirely masked; later queries are
                    // causal and each mask head also has finite negative bias.
                    float value = t == 0 || cell > t + 3 ? -INFINITY : -float(h) * 0.25f;
                    static_cast<ggml_fp16_t *>(mask->data)[(h * nq + t) * nk + cell] = ggml_fp32_to_fp16(value);
                }
    }
    require(ggml_backend_graph_compute(backend, graph) == GGML_STATUS_SUCCESS, "reference compute");
    std::vector<uint8_t> expected(ggml_nbytes(out));
    ggml_backend_tensor_get(out, expected.data(), 0, expected.size());
    zbKvEnable(false);
    require(ggml_backend_graph_compute(backend, graph) == GGML_STATUS_SUCCESS, "adapter compute");
    if (bias == 0 && (!masks || nq > 1))
        require(zbAttentionRows() > 0, "adapter path not exercised");
    if (bias != 0)
        require(zbAttentionRows() == 0, "unsupported bias should use upstream fallback");
    zbKvDisable();
    require(memcmp(expected.data(), out->data, expected.size()) == 0, "attention differs from upstream reference");
    ggml_backend_buffer_free(buffer);
    ggml_backend_free(backend);
    ggml_free(ctx);
}
int main() {
    try {
        unsigned cases = 0;
        for (int threads : {1, 2, 3}) {
            for (int nq : {1, 5, 17}) {
                for (int masks : {0, 1, 2}) {
                    check(nq, 33, 64, 64, 14, 2, threads, masks, 0);
                    ++cases;
                }
            }
        }
        check(7, 15, 6, 10, 6, 3, 2, 2, 0); // row length not divisible by 8 bytes
        check(5, 17, 64, 64, 14, 2, 3, 2, 1); // unsupported ALiBi fallback
        std::cout << "ATTENTION_REFERENCE_PASS cases=" << cases + 2 << " bit_exact=true\n";
        return 0;
    } catch (const std::exception &e) {
        std::cerr << e.what() << '\n';
        return 1;
    }
}
