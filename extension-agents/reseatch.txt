# Research verdict

**The video’s technical core is legitimate. The “Goodbye CUDA” framing is premature.**

The important development is not that Mojo and Vulkan have suddenly combined into a finished CUDA replacement. It is that several independent layers are converging:

1. **`llama.cpp` makes quantized local inference portable.**
2. **Vulkan gives `llama.cpp` a broadly cross-vendor GPU backend.**
3. **Mojo is attempting to make high-performance kernels portable across heterogeneous hardware.**
4. Together, they weaken CUDA’s lock on **consumer inference**, but they do not yet replace CUDA’s full training and production-serving ecosystem.

Based on the indexed description, the video specifically argues that `llama.cpp` can run GGUF models over Vulkan on consumer hardware, that an RX 7900 XTX can sometimes outperform ROCm through Vulkan, and that Mojo could become a Python-like, cross-hardware systems language. ([YouTube][1])

## Claim audit

| Claim                                      |                    Verdict | Qualification                                                                  |
| ------------------------------------------ | -------------------------: | ------------------------------------------------------------------------------ |
| Vulkan enables local AI on “any GPU”       |            **Mostly true** | Any reasonably modern, adequately supported Vulkan GPU—not literally every GPU |
| Vulkan can beat ROCm on AMD                | **True in some workloads** | Highly model-, operation-, driver-, and benchmark-dependent                    |
| Mojo runs like C++ with Python-like syntax |     **Directionally true** | Mojo remains beta and its GPU/runtime ecosystem is still developing            |
| Mojo + Vulkan replaces CUDA                |          **Not currently** | These are mostly separate portability paths                                    |
| CUDA’s moat is collapsing                  |              **Partially** | Most clearly in quantized local inference, not training or large-scale serving |

## 1. Vulkan really does broaden local-AI hardware support

`llama.cpp` has an official Vulkan backend with documented builds for Windows, Linux, Docker and macOS. On Linux and Windows it can offload model layers directly to a detected Vulkan GPU; the documentation even uses an Intel integrated GPU as its detection example. On macOS, Vulkan runs through translation layers that map Vulkan onto Metal. ([GitHub][2])

That means the same general GGUF inference engine can operate across:

* NVIDIA GPUs without using the CUDA backend
* AMD GPUs without depending on ROCm
* Intel Arc and integrated GPUs
* Some older or unusual GPUs with working Vulkan drivers
* macOS through Vulkan-to-Metal translation, although native Metal is usually the more sensible route

But **“any GPU” is marketing shorthand**. Actual usability depends on:

* Vulkan driver quality
* Required shader and matrix extensions
* Supported `llama.cpp` operators
* FP16 or integer capability
* VRAM or shared-memory capacity
* Whether operations silently fall back to the CPU
* The particular model architecture

Recent `llama.cpp` reports still show missing Vulkan kernels, Intel regressions, multi-GPU correctness bugs and model-specific CPU fallbacks. ([GitHub][3])

So the accurate claim is:

> Vulkan makes local inference available on a much larger range of GPUs through one backend, with uneven optimization and compatibility.

## 2. The RX 7900 XTX benchmark claim is credible—but narrow

A March 2026 `llama.cpp` report using identical `llama-bench` settings recorded approximately:

| Llama 7B Q4_0 token generation |           Throughput |
| ------------------------------ | -------------------: |
| Vulkan with Mesa RADV          | **167–177 tokens/s** |
| ROCm                           | **129–144 tokens/s** |

That directly supports the video’s claim that Vulkan can beat ROCm on an RX 7900 XTX for token generation. ([GitHub][4])

However, this does **not** establish that Vulkan is universally faster:

* ROCm may produce stronger prompt-processing throughput.
* Flash-attention implementations differ.
* Some models expose missing or less-optimized Vulkan operations.
* Driver and compiler versions materially affect results.
* Other RDNA3 testing has found ROCm faster than Vulkan under different configurations. ([GitHub][5])

The more precise conclusion is:

> On AMD consumer cards, Vulkan is no longer merely a compatibility fallback. For certain quantized decoding workloads, RADV Vulkan can outperform ROCm.

That is a significant development because Vulkan is generally easier to deploy across Windows and Linux than a tightly version-coupled ROCm environment.

## 3. Mojo is real, but the video likely overstates its present maturity

Mojo’s goal is to unify CPU, GPU and accelerator programming with a Python-derived language and a compiler stack based on LLVM and MLIR technologies. Modular explicitly describes its current focus as high-performance CPU and GPU programming. ([docs.modular.com][6])

As of July 2026:

* Mojo’s latest stable release is **1.0.0 beta 2**, released June 18, 2026.
* Its roadmap still labels the first high-performance CPU/GPU phase **“in progress.”**
* NVIDIA and AMD GPU programming are supported in the Modular stack.
* Apple Silicon GPU programming is functional, but large GenAI inference through MAX is not yet generally available there. ([docs.modular.com][7])

Mojo can export functions with the C ABI and build shared libraries, meaning Rust can call Mojo kernels through a conventional FFI boundary. ([docs.modular.com][8])

But **Mojo does not simply compile ordinary code to Vulkan/SPIR-V on every consumer GPU today**. Modular currently uses hardware-specific lowering paths for its supported targets. Vulkan is a separate open GPU API whose shaders are represented through SPIR-V. ([The Khronos Group][9])

There is a community `vulkan-mojo` binding, but it is currently:

* A low-level generated Vulkan binding
* Linux-only
* Explicitly unsafe
* Very small and early-stage
* Not a production local-LLM framework ([GitHub][10])

Therefore, **“Mojo + Vulkan” currently means architectural convergence more than a mature integrated product**.

## 4. CUDA is losing exclusivity, not disappearing

CUDA’s strongest moat is not merely its kernel language. It is the complete ecosystem:

* PyTorch’s mature CUDA execution path
* cuBLAS and specialized numerical libraries
* TensorRT and TensorRT-LLM
* NCCL multi-GPU communication
* Profiling, debugging and deployment tools
* Highly optimized kernels for NVIDIA tensor hardware

TensorRT remains an NVIDIA-specific inference optimizer requiring CUDA, with dedicated transformer, quantization and multi-GPU capabilities. ([NVIDIA Docs][11])

Vulkan attacks a different part of the market:

| Workload                                  | CUDA position       | Vulkan position                     |
| ----------------------------------------- | ------------------- | ----------------------------------- |
| Quantized single-user local LLM inference | Strong              | **Increasingly competitive**        |
| Consumer AMD/Intel compatibility          | Weak or unavailable | **Strong advantage**                |
| Cross-platform application distribution   | Vendor-specific     | **Strong advantage**                |
| Model training                            | Dominant            | Not a practical general replacement |
| High-throughput batched serving           | Dominant/mature     | Emerging                            |
| Multi-node and multi-GPU infrastructure   | Very mature         | Fragmented                          |
| Custom AI kernel development              | Mature, difficult   | Mojo may eventually change this     |

The CUDA moat is therefore **splitting**:

* The **local inference moat** is eroding rapidly.
* The **training and datacenter software moat** remains substantial.
* Mojo threatens the long-term programming-model moat, but it has not yet displaced CUDA.

# What this means for Ratty and your Bevy terminal

This is unusually relevant to your architecture, but not because you should rewrite Ratty in Mojo.

Bevy already uses `wgpu`, which selects native backends such as Vulkan, Metal and Direct3D. Your terminal renderer is already sitting above essentially the same cross-platform GPU abstraction strategy discussed in the video. ([Bevy Engine][12])

The strongest architecture would be:

```text
┌────────────────────────────────────────────────────┐
│ Ratty / Bevy ECS                                   │
│ terminals, spatial agents, effects, interaction    │
└──────────────────────┬─────────────────────────────┘
                       │ events / IPC / shared state
┌──────────────────────▼─────────────────────────────┐
│ Inference Runtime                                  │
│ llama.cpp backend selector                         │
│ Metal | Vulkan | CUDA | ROCm | CPU                 │
└──────────────────────┬─────────────────────────────┘
                       │ telemetry
┌──────────────────────▼─────────────────────────────┐
│ Agent Compute Scheduler                            │
│ token budget, VRAM budget, vision cadence, queues  │
└──────────────────────┬─────────────────────────────┘
                       │ optional C ABI
┌──────────────────────▼─────────────────────────────┐
│ Mojo Kernel Modules                                │
│ experimental perception / tensor / simulation ops  │
└────────────────────────────────────────────────────┘
```

### What to use now

**Keep Rust and Bevy as the control plane.** They are appropriate for ECS, windows, orbital position, synchronization, effects and deterministic simulation.

**Use `llama.cpp` as an inference sidecar or embedded library.** Dynamically select Metal, Vulkan, CUDA or another backend based on the machine.

**Treat Mojo as a pluggable compute laboratory.** Use it only for kernels where you can measure a meaningful gain—vision preprocessing, embeddings, spatial attention calculations, agent-neighborhood aggregation or tensor transforms.

**Do not build directly around the tiny `vulkan-mojo` project.** It is useful as evidence that Vulkan can be called from Mojo, not as a stable foundation.

## Specific implication for your Mac

For your current Mac-based development environment, **native Metal should remain the default inference backend**, not Vulkan. `llama.cpp` enables Metal by default on macOS, while its Vulkan route translates Vulkan calls back into Metal. ([GitHub][2])

Vulkan becomes strategically valuable when Ratty is distributed to:

* AMD Windows/Linux machines
* Intel Arc systems
* Mixed-GPU installations
* Affordable local-agent workstations
* Hardware where ROCm or CUDA installation is undesirable

## The genuinely novel opportunity

The most valuable idea is not merely running an LLM on another GPU. It is giving Ratty a **backend-neutral embodied compute layer**:

```text
agent perceives terminal + nearby agents
             ↓
vision/inference job enters GPU scheduler
             ↓
best available backend executes it
             ↓
result becomes ECS components:
AttentionTarget
Confidence
Intent
Mood
ObservedAgent
CoordinationVector
             ↓
Ratty renders the internal state physically
```

That allows the same agent society to run on Apple, AMD, Intel or NVIDIA hardware without changing its behavioral protocol.

Your “terminals hugging, high-fiving and observing one another” concept could then become actual multimodal coordination training: agents observe spatial states, emit actions, receive visual and protocol feedback, and learn policies over repeated interactions. Vulkan broadens where that experiment can run; Mojo could eventually accelerate the custom kernels; neither technology by itself provides the coordination-learning system.

# Final assessment

**Video accuracy:** 7.5/10
**Headline accuracy:** 5/10
**Technical significance:** High
**Production readiness of `llama.cpp` Vulkan:** Moderate to high
**Production readiness of Mojo GPU code:** Moderate/experimental
**Production readiness of direct Mojo–Vulkan local AI:** Low
**Relevance to Ratty:** Very high

The real takeaway is:

> CUDA is no longer required for useful local LLM inference. Vulkan is becoming a credible universal inference backend, while Mojo is a plausible future language for portable specialized kernels. They are complementary trends, not yet a unified CUDA replacement.

A useful next step is a Ratty backend specification that benchmarks Metal, Vulkan and CPU inference while exposing VRAM use, token throughput and queue contention as live ECS resources.

[1]: https://www.youtube.com/watch?v=oZagkCkBkww&utm_source=chatgpt.com "Mojo + Vulkan is INSANE: Run Local AI on ANY GPU ..."
[2]: https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md "llama.cpp/docs/build.md at master · ggml-org/llama.cpp · GitHub"
[3]: https://github.com/ggml-org/llama.cpp/issues/19957?utm_source=chatgpt.com "Qwen3.5-35B-A3B (qwen3_5moe) CPU-only on Vulkan · ..."
[4]: https://github.com/ggml-org/llama.cpp/issues/20934?utm_source=chatgpt.com "Misc. bug: [ROCm] Significantly lower token generation ..."
[5]: https://github.com/ggml-org/llama.cpp/discussions/12629?utm_source=chatgpt.com "Vulkan + AMD GPU + FlashAttention: Extreme performance ..."
[6]: https://docs.modular.com/mojo/faq?utm_source=chatgpt.com "Mojo FAQ"
[7]: https://docs.modular.com/mojo/roadmap/?utm_source=chatgpt.com "Mojo roadmap"
[8]: https://docs.modular.com/mojo/manual/python/mojo-from-python/?utm_source=chatgpt.com "Calling Mojo from Python"
[9]: https://www.khronos.org/spirv/?utm_source=chatgpt.com "SPIR: The Standard IR for Parallel Compute and Graphics"
[10]: https://github.com/Ryul0rd/vulkan-mojo?utm_source=chatgpt.com "Ryul0rd/vulkan-mojo"
[11]: https://docs.nvidia.com/deeplearning/tensorrt/latest/getting-started/support-matrix.html?utm_source=chatgpt.com "Support Matrix — NVIDIA TensorRT"
[12]: https://bevyengine.org/news/bevy-webgpu/?utm_source=chatgpt.com "Bevy + WebGPU"
