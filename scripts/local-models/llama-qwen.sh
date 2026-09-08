#!/usr/bin/env bash
# Qwen3.8-27B (Unsloth UD-Q6_K_XL, ~23 GB) whole on the A100: the per-turn
# narrator and referee, where latency is what the player waits on. Dense, so it
# is never split across cards. The MTP head ships as a separate draft GGUF and
# gives llama.cpp's draft-mtp speculative decoding ~35% more decode speed.
#
# The window is 262144 because that is the model's trained context and the
# ceiling llama-server enforces regardless: it caps n_ctx_slot at n_ctx_train
# and warns. Asking for 300k gets 262144 and a log line saying so.
#
# `-kvu` is what lets both slots see all of it. Unified, n_ctx_seq = n_ctx, so
# -c 262144 gives each slot a 262144 window out of one shared pool; without it
# n_ctx_seq = n_ctx / np, and two private 262144 halves would mean -c 524288.
# On a 40 GB card that is the whole difference. The model is hybrid --
# full_attention_interval 4, so only 16 of its 65 blocks carry a KV cache -- and
# at q8_0 those run ~34 KiB a token, the MTP draft ~4 more. 262144 tokens is
# ~9.5 GiB against the ~14 GiB free beside 24 GiB of weights; 524288 would be
# ~19 GiB and would not load. The shared pool is the trade: two long
# conversations at once divide it, where private halves would have guaranteed
# each one half and never more.
set -euo pipefail
M=/tank/data/Models/Qwen3.8-27B-GGUF
exec "/opt/llama.cpp/build/bin/llama-server" \
  -m "$M/Qwen3.8-27B-UD-Q6_K_XL.gguf" \
  -a qwen3.8-27b \
  --host 127.0.0.1 --port 8081 \
  -dev CUDA0 -ngl 99 \
  -c "${QWEN_CTX:-262144}" -np 2 -kvu -fa on -ctk q8_0 -ctv q8_0 \
  -t 16 -b 2048 -ub 512 \
  --spec-type draft-mtp -md "$M/MTP/mtp-Qwen3.8-27B-Q4_0.gguf" -devd CUDA0 \
  --jinja --reasoning-format deepseek --reasoning-budget -1 \
  --metrics
