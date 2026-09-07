#!/usr/bin/env bash
# DeepSeek V4 Flash-0731 (Unsloth UD-Q3_K_XL, 120 GB) on the three RTX 8000s.
#
# The experts are ~95% of the weights, so the quant decides everything: the
# UD-Q4_K_XL build (144 GB) could not fit the pool's 144 GB beside the KV
# cache and ran with `-ncmoe` keeping some layers' experts in system RAM,
# which cut prompt processing to ~19 tok/s. UD-Q3_K_XL fits whole, so
# DEEPSEEK_CPU_MOE defaults to 0; raise it only if a device reports
# out-of-memory at load. `--jinja` is what makes tool calling work, and
# `--reasoning-format deepseek` hands the thinking stream to Thetis as
# reasoning deltas rather than mixing it into the reply. DSpark is the model's
# own speculative-decoding draft: opt in with DEEPSEEK_DRAFT=1 once the build's
# draft-dspark path is proven — on 2026-09-06 (llama.cpp 0624065) it crashed in
# common_speculative_init_result after the main model had loaded.
set -euo pipefail
M=/tank/data/Models/DeepSeek-V4-Flash-0731-GGUF
exec "$HOME/llama.cpp/build/bin/llama-server" \
  -m "$M/${DEEPSEEK_QUANT:-UD-Q3_K_XL}/DeepSeek-V4-Flash-0731-${DEEPSEEK_QUANT:-UD-Q3_K_XL}-00001-of-0000${DEEPSEEK_SHARDS:-4}.gguf" \
  -a deepseek-v4-flash \
  --host 127.0.0.1 --port 8080 \
  -dev CUDA1,CUDA2,CUDA3 -ngl 99 -ts "${DEEPSEEK_SPLIT:-1,1,1}" -ncmoe "${DEEPSEEK_CPU_MOE:-0}" \
  -c "${DEEPSEEK_CTX:-131072}" -np 2 -fa auto \
  -t 40 -b 2048 -ub 512 \
  ${DEEPSEEK_DRAFT:+--spec-type draft-dspark -md "$M/dspark-DeepSeek-V4-Flash-0731-Q8_0.gguf" -devd CUDA1} \
  --jinja --reasoning-format deepseek --reasoning-budget -1 \
  --metrics
