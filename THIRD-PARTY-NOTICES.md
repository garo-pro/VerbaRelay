# Third-party notices

VerbaRelay itself is licensed under the MIT license (see `LICENSE`). Release
archives contain the following third-party components.

## whisper.cpp and ggml

Copyright (c) 2023-2026 The ggml authors. MIT license.
https://github.com/ggml-org/whisper.cpp

## libopus (statically linked into the server)

Copyright (c) 2001-2023 Xiph.Org, Skype Limited, Octasic, Jean-Marc Valin,
Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark Borgerding, Erik de Castro
Lopo, Mozilla, Amazon. BSD 3-clause license.
https://opus-codec.org/license/

symphonia-adapter-libopus: MIT or Apache-2.0. opusic-sys: BSD 3-clause.
https://github.com/aschey/symphonia-adapters

## wxWidgets (statically linked into the client through wxDragon)

wxWindows Library Licence, Version 3.1 (LGPL with an exception that permits
distribution of binaries linked with wxWidgets under the licensee's own terms).
https://www.wxwidgets.org/about/licence/

wxDragon: MIT or Apache-2.0. https://github.com/AllenDang/wxDragon

## NVIDIA CUDA runtime and cuBLAS (CUDA package only)

The CUDA package contains `cudart`, `cublas` and `cublasLt`, redistributed under
the terms of the NVIDIA CUDA Toolkit End User License Agreement, which lists
these files as redistributable. https://docs.nvidia.com/cuda/eula/

## Microsoft Visual C++ runtime (Windows only)

`msvcp140.dll`, `vcruntime140.dll` and `vcruntime140_1.dll` are distributed
under the Microsoft Visual Studio redistribution terms.

## Benchmark sample

`crates/client/assets/benchmark.wav` is the 11 second excerpt of John F.
Kennedy's inaugural address (1961) distributed with whisper.cpp. The recording
is a work of the United States government and in the public domain.

## Whisper models

Models are not bundled; peers download them from
https://huggingface.co/ggerganov/whisper.cpp. The Whisper model weights by
OpenAI are released under the MIT license.

## Rust crates

The client and server are built from many Rust crates under MIT, Apache-2.0
and similar permissive licenses. `cargo tree` lists them; `cargo about` can
generate a full report.

## Design credit

The updater design (semantic versions for stable releases, commit hashes for
development builds, minisign verification) is inspired by ship-shape by
trypsynth (MIT), https://github.com/trypsynth/ship-shape. No code was copied.
