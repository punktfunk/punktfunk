#!/usr/bin/env bash
# Recompile the presenter's shaders to the committed .spv blobs. The blobs are committed
# so neither developer builds nor CI need a SPIR-V toolchain (glslc is a dev-time tool);
# rerun this after editing a .vert/.frag and commit both.
set -euo pipefail
cd "$(dirname "$0")"
for f in *.vert *.frag; do
    glslc -O "$f" -o "$f.spv"
    echo "compiled $f -> $f.spv"
done
glslc -O chroma_interleave.comp -o chroma_rg8.comp.spv
glslc -O -DWIDE chroma_interleave.comp -o chroma_rg16.comp.spv
echo "compiled chroma_interleave.comp -> chroma_rg8/rg16.comp.spv"
