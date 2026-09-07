#!/usr/bin/env bash
# DeepSeek V4 Flash-0731 (Unsloth UD-Q4_K_XL, 155 GB) on the three RTX 8000s.
#
# The experts are ~95% of the weights (43 layers x 3 MXFP4 expert tensors of
# ~1.1 GB each), so the model does not fit in the pool's 144 GB beside the KV
# cache. `-ncmoe` keeps the first N layers' experts in system RAM instead (the
# host has 376 GB); raise it if a device reports out-of-memory at load, lower
# it once headroom is proven. `--jinja` is what makes tool calling work, and
# `--reasoning-format deepseek` hands the thinking stream to Thetis as
# reasoning deltas rather than mixing it into the reply. DSpark is the model's
# own speculative-decoding draft; it lives on the first pool card.
set -euo pipefail
M=/tank/data/Models/DeepSeek-V4-Flash-0731-GGUF
exec "$HOME/llama.cpp/build/bin/llama-server" \
  -m "$M/UD-Q4_K_XL/DeepSeek-V4-Flash-0731-UD-Q4_K_XL-00001-of-00005.gguf" \
  -a deepseek-v4-flash \
  --host 127.0.0.1 --port 8080 \
  -dev CUDA1,CUDA2,CUDA3 -ngl 99 -ts 1,1,1 -ncmoe "${DEEPSEEK_CPU_MOE:-14}" \
  -c "${DEEPSEEK_CTX:-131072}" -np 2 -fa auto \
  -t 40 -b 2048 -ub 512 \
  --spec-type draft-dspark -md "$M/dspark-DeepSeek-V4-Flash-0731-Q8_0.gguf" -devd CUDA1 \
  --jinja --reasoning-format deepseek --reasoning-budget -1 \
  --metrics
