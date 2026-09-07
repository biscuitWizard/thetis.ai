#!/usr/bin/env bash
# Qwen3.8-27B (Unsloth UD-Q6_K_XL, ~23 GB) whole on the A100: the per-turn
# narrator and referee, where latency is what the player waits on. Dense, so it
# is never split across cards. The MTP head ships as a separate draft GGUF and
# gives llama.cpp's draft-mtp speculative decoding ~35% more decode speed.
set -euo pipefail
M=/tank/data/Models/Qwen3.8-27B-GGUF
exec "$HOME/llama.cpp/build/bin/llama-server" \
  -m "$M/Qwen3.8-27B-UD-Q6_K_XL.gguf" \
  -a qwen3.8-27b \
  --host 127.0.0.1 --port 8081 \
  -dev CUDA0 -ngl 99 \
  -c "${QWEN_CTX:-131072}" -np 2 -fa on -ctk q8_0 -ctv q8_0 \
  -t 16 -b 2048 -ub 512 \
  --spec-type draft-mtp -md "$M/MTP/mtp-Qwen3.8-27B-Q4_0.gguf" -devd CUDA0 \
  --jinja --reasoning-format deepseek --reasoning-budget -1 \
  --metrics
