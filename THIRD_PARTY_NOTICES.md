# Third-party notices

RoomMesh is licensed under the GNU General Public License v3.0 (see [`LICENSE`](LICENSE)). It
includes or links the third-party components below. All of them are under permissive licenses that
are compatible with GPL-3.0, and each remains under its own license.

## Main components

| Component | Used for | License | Source |
|---|---|---|---|
| [libASPL](https://github.com/gavv/libASPL) 3.1.2 | The CoreAudio HAL plug-in framework under `RoomMesh.driver` (statically linked) | MIT | fetched by `driver/CMakeLists.txt` |
| [WebRTC Audio Processing](https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing) (APM), through the [`webrtc-audio-processing`](https://crates.io/crates/webrtc-audio-processing) crates 2.1 | Echo cancellation, noise suppression and gain control in the core (statically linked) | BSD-3-Clause | bundled by `webrtc-audio-processing-sys` |
| [Abseil C++](https://github.com/abseil/abseil-cpp) 20240722 | A dependency of WebRTC APM (statically linked) | Apache-2.0 | a Meson subproject of WebRTC APM |
| [libopus](https://opus-codec.org) (Opus codec), through the [`opus`](https://crates.io/crates/opus) 0.4 (MIT OR Apache-2.0) and [`opusic-sys`](https://crates.io/crates/opusic-sys) crates | Audio compression between Macs (statically linked) | BSD-3-Clause | bundled by `opusic-sys` |
| [UniFFI](https://github.com/mozilla/uniffi-rs) 0.32 | The Rust ↔ Swift bindings (runtime crates and generated code) | MPL-2.0 | crates.io |
| [cpal](https://github.com/RustAudio/cpal) 0.18 | Audio device access in the core and `roommesh-devtool` | Apache-2.0 | crates.io |

The Rust core's other dependencies come from crates.io under MIT, Apache-2.0, BSD-3-Clause
(`curve25519-dalek`, `x25519-dalek`, `subtle`), ISC, Zlib or Unlicense terms, most of them dual
licensed "MIT OR Apache-2.0". `core/Cargo.lock` pins the exact versions, and each crate's license
ships in its crates.io package. To list them all, run for example
`cargo install cargo-license && cargo license --manifest-path core/Cargo.toml`.

The full texts of the Apache License 2.0 and the Mozilla Public License 2.0 are at
<https://www.apache.org/licenses/LICENSE-2.0> and <https://www.mozilla.org/MPL/2.0/>. The MIT and
BSD-3-Clause notices that binary redistributions must reproduce follow.

## libASPL (MIT)

```text
The MIT License (MIT)

Copyright (c) Victor Gaydov and contributors.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## WebRTC Audio Processing (BSD-3-Clause)

```text
Copyright (c) 2011, Google Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of Google nor the names of its contributors may
    be used to endorse or promote products derived from this software
    without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## libopus (BSD-3-Clause)

```text
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

## curve25519-dalek, x25519-dalek, subtle (BSD-3-Clause)

These crates provide the key exchange behind RoomMesh's encrypted connections. They are under the
same BSD-3-Clause terms as above, with these copyright holders (the full text is in each crate's
`LICENSE` file):

```text
curve25519-dalek: Copyright (c) 2016-2021 isis agora lovecruft. All rights reserved.
                  Copyright (c) 2016-2021 Henry de Valence. All rights reserved.
x25519-dalek:     Copyright (c) 2017-2021 isis agora lovecruft. All rights reserved.
                  Copyright (c) 2019-2021 DebugSteven. All rights reserved.
subtle:           Copyright (c) 2016-2017 Isis Agora Lovecruft, Henry de Valence. All rights reserved.
                  Copyright (c) 2016-2024 Isis Agora Lovecruft. All rights reserved.
```
