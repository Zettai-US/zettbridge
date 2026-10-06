// SPDX-License-Identifier: MIT
// CPU GQA loop interchange for UC memory. Persistent K/V tensor addresses never
// change. Only one K row and one V row are read into per-thread arithmetic
// scratch, then consumed by all query heads sharing that KV head.
#include "ggml-cpu/vec.h"
#include "kv_buffer.h"
#include <algorithm>
#include <atomic>
#include <cmath>
#include <vector>

static std::atomic<uint64_t> kv_rows_read{0};
uint64_t zbAttentionRows() { return kv_rows_read.load(std::memory_order_relaxed); }
void zbAttentionReset() { kv_rows_read.store(0, std::memory_order_relaxed); }

extern "C" void *__wrap_memcpy(void *, const void *, size_t);
extern "C" void __real_ggml_compute_forward_flash_attn_ext(const ggml_compute_params *, ggml_tensor *);

extern "C" void __wrap_ggml_compute_forward_flash_attn_ext(const ggml_compute_params *params, ggml_tensor *dst) {
    auto reference = *params;
    reference.use_ref = true;
    if (!zbKvActive()) {
        // The unmodified upstream scalar FA is the independent reference. It
        // uses the same rounding order for prefill and single-token decode.
        __real_ggml_compute_forward_flash_attn_ext(&reference, dst);
        return;
    }
    const auto *q = dst->src[0], *k = dst->src[1], *v = dst->src[2], *mask = dst->src[3];
    float scale, bias, softcap;
    memcpy(&scale, dst->op_params, sizeof(float));
    memcpy(&bias, dst->op_params + 1, sizeof(float));
    memcpy(&softcap, dst->op_params + 2, sizeof(float));
    // Keep the adapter bounded to the tested dense GQA layout. Other layouts
    // retain the upstream path and remain directly backed by their KV buffer.
    if (q->type != GGML_TYPE_F32 || k->type != GGML_TYPE_F16 || v->type != GGML_TYPE_F16 || q->ne[3] != 1 ||
        k->ne[3] != 1 || v->ne[3] != 1 || k->ne[2] != v->ne[2] || k->ne[1] != v->ne[1] || q->ne[2] % k->ne[2] ||
        dst->src[4] || bias || softcap || (mask && mask->type != GGML_TYPE_F16) || q->nb[0] != 4 || k->nb[0] != 2 ||
        v->nb[0] != 2 || k->ne[0] > 256 || v->ne[0] > 256) {
        __real_ggml_compute_forward_flash_attn_ext(&reference, dst);
        return;
    }
    const int dk = k->ne[0], dv = v->ne[0], nq = q->ne[1], group = q->ne[2] / k->ne[2];
    const int rows = nq * group;
    // Q and attention-output accumulators are ordinary compute scratch, not KV.
    std::vector<ggml_fp16_t> query(rows * dk), accumulator(rows * dv);
    std::vector<float> maximum(rows), sum(rows);
    std::vector<ggml_fp16_t> key(dk), value(dv); // 256 bytes total for Qwen2.5-0.5B
    for (int head = params->ith; head < k->ne[2]; head += params->nth) {
        std::fill(maximum.begin(), maximum.end(), -INFINITY);
        std::fill(sum.begin(), sum.end(), 0.0f);
        std::fill(accumulator.begin(), accumulator.end(), ggml_fp16_t(0));
        for (int h = 0; h < group; ++h) {
            for (int t = 0; t < nq; ++t) {
                auto *src = reinterpret_cast<const float *>(static_cast<const char *>(q->data) + t * q->nb[1] +
                                                            (head * group + h) * q->nb[2]);
                ggml_cpu_fp32_to_fp16(src, query.data() + (h * nq + t) * dk, dk);
            }
        }
        auto maskAt = [&](int h, int t, int cell) {
            if (!mask)
                return 0.0f;
            auto *row = reinterpret_cast<const ggml_fp16_t *>(static_cast<const char *>(mask->data) + t * mask->nb[1] +
                                                              ((head * group + h) % mask->ne[2]) * mask->nb[2]);
            return GGML_CPU_FP16_TO_FP32(row[cell]);
        };
        for (int cell = 0; cell < k->ne[1]; ++cell) {
            bool needed = false;
            for (int h = 0; h < group && !needed; ++h)
                for (int t = 0; t < nq && !needed; ++t)
                    needed = maskAt(h, t, cell) != -INFINITY;
            if (!needed)
                continue;
            __wrap_memcpy(key.data(), static_cast<const char *>(k->data) + cell * k->nb[1] + head * k->nb[2], dk * 2);
            __wrap_memcpy(value.data(), static_cast<const char *>(v->data) + cell * v->nb[1] + head * v->nb[2], dv * 2);
            kv_rows_read.fetch_add(1, std::memory_order_relaxed);
            for (int h = 0; h < group; ++h) {
                for (int t = 0; t < nq; ++t) {
                    const float mv = maskAt(h, t, cell);
                    if (mv == -INFINITY)
                        continue;
                    const int row = h * nq + t;
                    auto *acc = accumulator.data() + row * dv;
                    float score;
                    ggml_vec_dot_f16(dk, &score, 0, key.data(), 0, query.data() + row * dk, 0, 1);
                    score = score * scale + mv;
                    float ms = 1.0f, vs = 1.0f;
                    if (score > maximum[row]) {
                        ms = expf(maximum[row] - score);
                        maximum[row] = score;
                        ggml_vec_scale_f16(dv, acc, ms);
                    } else {
                        vs = expf(score - maximum[row]);
                    }
                    ggml_vec_mad_f16(dv, acc, value.data(), vs);
                    sum[row] = sum[row] * ms + vs;
                }
            }
        }
        for (int h = 0; h < group; ++h) {
            for (int t = 0; t < nq; ++t) {
                const int row = h * nq + t;
                const float inv = sum[row] == 0 ? 0.0f : 1.0f / sum[row];
                auto *output = reinterpret_cast<float *>(static_cast<char *>(dst->data) +
                                                         (head * group + h + t * dst->ne[1]) * dst->nb[1]);
                for (int d = 0; d < dv; ++d)
                    output[d] = GGML_CPU_FP16_TO_FP32(accumulator[row * dv + d]) * inv;
            }
        }
    }
}
