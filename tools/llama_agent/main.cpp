// SPDX-License-Identifier: MIT
// Standalone agent using unmodified llama.cpp and a direct ggml KV buffer.
#include "kv_buffer.h"
#include "llama.h"
#include <algorithm>
#include <chrono>
#include <cstring>
#include <fcntl.h>
#include <fstream>
#include <iostream>
#include <memory>
#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <thread>
#include <unistd.h>
#include <vector>

using Json = nlohmann::ordered_json;
using Clock = std::chrono::steady_clock;
using Context = std::unique_ptr<llama_context, decltype(&llama_free)>;
using Model = std::unique_ptr<llama_model, decltype(&llama_model_free)>;
using Sampler = std::unique_ptr<llama_sampler, decltype(&llama_sampler_free)>;

static void require(bool ok, const std::string &message) {
    if (!ok)
        throw std::runtime_error(message);
}
static void event(const Json &value) { std::cout << value.dump() << std::endl; }
static double seconds(Clock::time_point start) { return std::chrono::duration<double>(Clock::now() - start).count(); }
static Context makeContext(llama_model *model) {
    auto params = llama_context_default_params();
    params.n_ctx = 256;
    params.n_batch = 256;
    params.n_ubatch = 256;
    params.n_threads = params.n_threads_batch = 2;
    params.offload_kqv = false;
    params.cb_eval = zbKvEval;
    params.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_ENABLED;
    Context ctx(llama_init_from_model(model, params), llama_free);
    require(bool(ctx), "create llama context");
    return ctx;
}
static void decode(llama_context *ctx, std::vector<llama_token> &tokens) {
    for (size_t offset = 0; offset < tokens.size(); offset += 256) {
        auto count = std::min<size_t>(256, tokens.size() - offset);
        auto batch = llama_batch_get_one(tokens.data() + offset, static_cast<int32_t>(count));
        require(llama_decode(ctx, batch) == 0, "llama_decode failed or context exhausted");
    }
}
static void append(llama_context *ctx, const llama_vocab *vocab, const std::string &text) {
    int count = -llama_tokenize(vocab, text.c_str(), text.size(), nullptr, 0, false, true);
    require(count > 0, "empty input tokenization");
    std::vector<llama_token> tokens(count);
    require(llama_tokenize(vocab, text.c_str(), text.size(), tokens.data(), count, false, true) == count,
            "tokenization size changed");
    decode(ctx, tokens);
}
static const char *grammar = R"GBNF(
root ::= "{\"tool\":" ("\"read_order\",\"id\":" string | "\"multiply\",\"a\":" integer ",\"b\":" integer | "\"final\",\"answer\":" integer) "}"
integer ::= [0-9]{1,8}
string ::= "\"" [a-zA-Z0-9_-]{1,32} "\""
)GBNF";

static Json generate(llama_context *ctx, const llama_vocab *vocab) {
    Sampler sampler(llama_sampler_chain_init(llama_sampler_chain_default_params()), llama_sampler_free);
    auto constrained = llama_sampler_init_grammar(vocab, grammar, "root");
    require(constrained, "invalid tool schema grammar");
    llama_sampler_chain_add(sampler.get(), constrained);
    llama_sampler_chain_add(sampler.get(), llama_sampler_init_greedy());
    std::string text;
    std::vector<llama_token> generated;
    std::vector<uint64_t> logits_hashes;
    for (unsigned i = 0; i < 96; ++i) {
        uint64_t hash = 14695981039346656037ULL;
        const auto *raw = reinterpret_cast<const uint8_t *>(llama_get_logits_ith(ctx, -1));
        for (size_t j = 0; j < llama_vocab_n_tokens(vocab) * sizeof(float); ++j) {
            hash = (hash ^ raw[j]) * 1099511628211ULL;
        }
        logits_hashes.push_back(hash);
        llama_token token = llama_sampler_sample(sampler.get(), ctx, -1);
        if (llama_vocab_is_eog(vocab, token))
            break;
        char piece[256];
        int size = llama_token_to_piece(vocab, token, piece, sizeof(piece), 0, false);
        require(size >= 0, "token piece too large");
        text.append(piece, size);
        generated.push_back(token);
        std::vector<llama_token> one{token};
        decode(ctx, one);
        if (!text.empty() && text.back() == '}')
            break;
    }
    Json call = Json::parse(text);
    return {{"call", call}, {"tokens", generated}, {"text", text}, {"logits_hashes", logits_hashes}};
}

static Json run(llama_model *model, const Json &order, const std::string &mode) {
    const auto *vocab = llama_model_get_vocab(model);
    if (mode != "resident")
        zbKvEnable(mode == "cxl");
    Context ctx = makeContext(model);
    const std::string prefix = "<|im_start|>assistant\n";
    const std::string system = "Return JSON tool calls. First read_order(id), then multiply(quantity,unit_price), "
                               "then final(answer) with the tool result. "
                               "Examples: {\"tool\":\"read_order\",\"id\":\"x\"}, "
                               "{\"tool\":\"multiply\",\"a\":2,\"b\":3}, {\"tool\":\"final\",\"answer\":6}.";
    append(ctx.get(), vocab,
           "<|im_start|>system\n" + system +
               "<|im_end|>\n<|im_start|>user\n"
               "Calculate the total for order ID " +
               order.at("id").dump() + ". Use the exact ID.<|im_end|>\n" + prefix);
    Json trace = Json::array();
    bool read = false, multiplied = false, finished = false;
    for (unsigned round = 0; round < 5; ++round) {
        Json output = generate(ctx.get(), vocab);
        Json call = output["call"];
        trace.push_back(output);
        event({{"event", "MODEL_TURN"}, {"mode", mode}, {"round", round}, {"output", output}});
        std::string tool = call.at("tool");
        if (tool == "final") {
            require(read && multiplied, "model skipped required tools");
            require(call.at("answer").get<int64_t>() ==
                        order.at("quantity").get<int64_t>() * order.at("unit_price").get<int64_t>(),
                    "agent returned wrong total");
            finished = true;
            break;
        }
        if (mode != "resident")
            zbKvVerify(round);
        Json result;
        if (tool == "read_order") {
            require(!read && call.at("id") == order.at("id"), "invalid order lookup");
            read = true;
            result = order;
        } else if (tool == "multiply") {
            require(read && !multiplied, "invalid tool ordering");
            const int64_t a = call.at("a"), b = call.at("b");
            const int64_t qty = order.at("quantity"), price = order.at("unit_price");
            require((a == qty && b == price) || (a == price && b == qty), "model used wrong tool arguments");
            multiplied = true;
            result = {{"result", a * b}};
        } else {
            throw std::runtime_error("unknown tool");
        }
        // KV remains mapped in the same buffer across tool waits and decode.
        std::this_thread::sleep_for(std::chrono::seconds(1));
        event({{"event", "TOOL_RESULT"}, {"mode", mode}, {"round", round}, {"result", result}});
        append(ctx.get(), vocab,
               "<|im_end|>\n<|im_start|>user\nTool result: " + result.dump() + "<|im_end|>\n" + prefix);
    }
    require(finished && trace.size() == 3, "agent did not finish within the tool budget");
    if (mode != "resident")
        zbKvVerify(2);
    ctx.reset();
    if (mode != "resident")
        zbKvDisable();
    return trace;
}

int main(int argc, char **argv) {
    try {
        require(argc == 4, "usage: zb-llama-agent MODEL.gguf ORDER.json local|cxl");
        std::string mode(argv[3]);
        require(mode == "local" || mode == "cxl", "unknown mode");
        std::ifstream input(argv[2]);
        Json order = Json::parse(input);
        auto start = Clock::now();
        llama_backend_init();
        auto params = llama_model_default_params();
        params.n_gpu_layers = 0;
        Model model(llama_model_load_from_file(argv[1], params), llama_model_free);
        require(bool(model), "load model");
        Json reference = run(model.get(), order, "resident");
        Json resumed = run(model.get(), order, mode);
        require(reference == resumed, "direct KV agent token trace differs from resident reference");
        event({{"event", "AGENT_E2E_PASS"},
               {"mode", mode},
               {"tool_calls", 2},
               {"model_turns", 3},
               {"direct_kv", true},
               {"token_exact_match", true},
               {"logits_hash_match", true},
               {"seconds", seconds(start)},
               {"answer", resumed.back()["call"]["answer"]}});
        model.reset();
        llama_backend_free();
        return 0;
    } catch (const std::exception &e) {
        event({{"event", "AGENT_E2E_FAIL"}, {"error", e.what()}});
        return 1;
    }
}
