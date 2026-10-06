// SPDX-License-Identifier: MIT
#pragma once
#include "ggml.h"

void zbKvEnable(bool remote);
void zbKvDisable();
void zbKvVerify(unsigned round);
bool zbKvActive();
uint64_t zbAttentionRows();
void zbAttentionReset();
bool zbKvEval(ggml_tensor *tensor, bool ask, void *unused);
