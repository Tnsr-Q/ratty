Question:

I'm assuming you still think I should Use llama.cpp as an inference sidecar or embedded library. Dynamically select Metal, Vulkan, CUDA or another backend based on the machine. Treat Mojo as a pluggable compute laboratory. Use it only for kernels where you can measure a meaningful gain—vision preprocessing, embeddings, spatial attention calculations, agent-neighborhood aggregation or tensor transforms.


Answer:

Yes. **AirLLM materially changes the analysis.** It is the technology that supports the “DeepSeek-V3 on approximately 12 GB” claim.

But it changes the answer in a specific way:

> **AirLLM makes extremely large models technically executable on small-memory hardware. It does not make them fast, interactive, or suitable as the primary runtime for 20 concurrent agents.**

# What AirLLM actually does

AirLLM creates the model structure on a metadata-only device, splits the checkpoint into layer-level files, and then repeatedly:

1. Loads one layer from disk into CPU memory.
2. Transfers that layer to the GPU.
3. Executes it.
4. Evicts it back to the metadata device.
5. Loads the next layer.

The current implementation installs forward hooks that stream each complete model module immediately before execution and remove it afterward.

This means its peak accelerator-memory requirement is determined primarily by the **largest individual layer**, not the complete model.

The project now explicitly claims:

* 70B models on approximately 4 GB
* Llama 3.1 405B on approximately 8 GB
* DeepSeek-V3 671B on approximately 12 GB

DeepSeek-V3 support was marked complete in AirLLM 3.0.1 through the generic Transformers streaming path with native handling for its FP8 checkpoint.

# The critical qualification

**The 12 GB figure refers to the active working set—not the amount of storage required.**

DeepSeek-V3 contains 671B main-model parameters, with 37B activated per token, and its distributed checkpoint includes approximately 685B parameters when the auxiliary multi-token-prediction module is counted. ([Hugging Face][1])

AirLLM’s README warns that the original checkpoint must first be downloaded and decomposed into layer-level shards, requiring substantial disk space. Its utility code explicitly checks that enough free disk exists to create another model-sized collection of split files; `delete_original=True` can remove the original afterward.

For DeepSeek-V3, expect:

* **Hundreds of gigabytes of download**
* Roughly another model-sized allocation while splitting
* A very large NVMe storage requirement
* Heavy disk bandwidth during every generation

So the more complete description is:

> **DeepSeek-V3 can potentially execute with approximately 12 GB of peak GPU memory, provided you have roughly terabyte-class fast storage and accept extremely low throughput.**

# Why it will be slow

During autoregressive generation, every new token requires another forward pass through the model.

AirLLM’s generic implementation loads each complete layer state before that layer runs. Afterward, it evicts that layer.

Its macOS implementation is even more explicit: after generating the first token, the loop recreates and reloads every transformer layer from persistent storage for each subsequent token.

For a checkpoint on the order of 700 GB, even an unrealistically clean sequential-read calculation gives:

| Sustained storage bandwidth | Theoretical full-model read time |
| --------------------------: | -------------------------------: |
|                      3 GB/s |                     ~233 seconds |
|                      5 GB/s |                     ~140 seconds |
|                      7 GB/s |                     ~100 seconds |

Real behavior could differ because of caching, shard layout, expert architecture and prefetching. But the code demonstrates that disk transfer is the dominant design constraint.

AirLLM’s README says its weight compression helps specifically because “the bottleneck is mainly at the disk loading.”

The repository currently does not publish a reproducible DeepSeek-V3 benchmark with:

* Tokens per second
* Time to first token
* Exact GPU and SSD
* Disk usage
* Peak RAM and VRAM
* Context length
* Output validation

An open repository issue specifically flags that the headline memory claims lack reproducible timing and hardware evidence.

Therefore, I consider the 12 GB claim **architecturally plausible but operationally unproven**.

# MoE does not completely rescue it

DeepSeek-V3 activates only about 37B parameters for each token, even though it contains 671B total parameters. ([Hugging Face][1])

That normally creates a major computational advantage because only selected experts execute. But AirLLM’s current generic loading hook loads the state dictionary for the entire transformer layer before the model performs expert selection.

Based on that implementation, AirLLM appears to stream **all experts contained in each layer**, rather than reading only the experts selected for the current token. That is an architectural inference from the current code, not a published AirLLM benchmark.

A future expert-aware loader could be much more interesting:

```text
router computes selected experts
          ↓
load only selected expert weights
          ↓
execute selected experts
          ↓
retain frequently selected experts in cache
```

That would move AirLLM from layer streaming toward **expert streaming**, which could be genuinely powerful for MoE agent systems.

# AirLLM versus llama.cpp and Ollama

They optimize for almost opposite objectives.

| Property                  | AirLLM                             | llama.cpp / Ollama                          |
| ------------------------- | ---------------------------------- | ------------------------------------------- |
| Primary goal              | Run models larger than memory      | Fast usable local inference                 |
| Weight strategy           | Stream layers from disk repeatedly | Keep quantized weights resident in RAM/VRAM |
| Model format              | Hugging Face/PyTorch safetensors   | Usually GGUF or managed Ollama model        |
| Memory requirement        | Approximately one layer            | Most or all quantized model weights         |
| Storage I/O               | Extremely high                     | Primarily load-time                         |
| Interactive throughput    | Generally poor for huge models     | Generally much better                       |
| Concurrent agents         | Poor fit                           | Better fit                                  |
| Metal                     | Separate MLX code path             | First-class in llama.cpp                    |
| CUDA                      | PyTorch CUDA path                  | Native CUDA kernels                         |
| Vulkan                    | No direct AirLLM backend           | Supported by llama.cpp                      |
| Embedded Rust integration | Difficult/Python boundary          | Practical through C API or server           |
| Model compatibility       | Broad Transformers compatibility   | Requires model conversion/support           |
| Best role                 | Offline oversized-model execution  | Continuous inference service                |

`llama.cpp` explicitly supports Metal, CUDA, HIP, Vulkan, SYCL and CPU/GPU hybrid inference, with low-bit quantization intended for reduced memory and faster inference. ([GitHub][2])

AirLLM does **not** replace the cross-platform backend selection architecture. Its primary generic path is PyTorch and defaults to `cuda:0`; macOS uses a separate MLX implementation.

# Important macOS warning

On Darwin, `AutoModel.from_pretrained()` always selects `AirLLMLlamaMlx`, regardless of the model architecture.

That MLX implementation defines a conventional dense Llama-style transformer:

* Standard attention projections
* A dense gated feed-forward block
* No visible DeepSeekMoE expert router
* No DeepSeek MLA-specific implementation

Therefore:

> **I would not assume that full DeepSeek-V3 works correctly through AirLLM on your Mac merely because AirLLM 3.0.1 supports DeepSeek-V3 on its generic path.**

The current code suggests that the strongest DeepSeek-V3 support is likely the Linux/PyTorch/CUDA path. The repository contains multiple historical macOS/MLX compatibility reports, and its DeepSeek-V3 claim is not accompanied by a macOS demonstration.

This should be tested with a much smaller DeepSeek-family checkpoint before downloading the full model.

# Revised architecture for your system

I would now use **three inference temperatures**.

## 1. Hot path: llama.cpp or Ollama

For always-on agents:

* Conversation
* Tool selection
* Vision descriptions
* Memory classification
* Embeddings
* Spatial coordination
* Code assistance
* Fast reflection

Use models small enough to remain resident.

```text
latency target: milliseconds to several seconds
frequency: continuous
concurrency: many agents
```

## 2. Warm path: larger quantized resident models

For harder tasks:

* Planning
* Reconciliation
* Coding
* Session summarization
* Contradiction analysis
* Critic agents

Use a larger GGUF model partially offloaded between RAM and GPU.

```text
latency target: seconds to perhaps a minute
frequency: selective
concurrency: limited
```

## 3. Cold path: AirLLM

Use AirLLM for work where quality matters more than latency:

* Overnight memory consolidation
* Offline evaluation
* Generating synthetic training examples
* Deep repository review
* Comparing a local skill against a large reference model
* Adjudicating agent disagreements
* Producing a high-quality session synthesis
* Running proof-of-evaluation batches
* Testing whether a smaller agent missed an important conclusion

```text
latency target: minutes or longer
frequency: rare or batch
concurrency: one job per storage channel
```

This gives AirLLM an important role without letting it stall your agent society.

# Where it fits with The-comune and SparrowDB

AirLLM should be represented as a specialized **slow expert node**:

```text
fast local agents
      ↓
submit difficult unresolved claim
      ↓
The-comune signs and queues deliberation request
      ↓
AirLLM cold worker processes request
      ↓
result becomes a proposed Assessment
      ↓
SparrowDB stores result + provenance
      ↓
belief layer decides whether to surface it
```

A useful graph model would be:

```text
Question
  ├──PROPOSED_ANSWER──> FastAgentAnswer
  ├──ESCALATED_TO─────> AirLLMWorker
  └──RESOLVED_BY──────> Assessment

Assessment
  ├──DERIVED_FROM─────> SessionRoot
  ├──EVALUATES────────> FastAgentAnswer
  ├──GENERATED_BY─────> DeepSeekV3
  └──OBSERVED_BY──────> AirLLMWorker
```

The result should remain a claim, not automatically become system truth.

# How to expose AirLLM to Ratty

I would wrap it as a separate Python worker rather than embed it in the Rust process:

```text
Ratty / Rust
     │
     │ enqueue job
     ▼
NATS or local durable queue
     │
     ▼
AirLLM Python worker
     │
     ├── model shard storage
     ├── profiling
     ├── cancellation
     └── result stream
     │
     ▼
PCKP result packet
```

The monitoring dashboard could show:

* Current layer
* Layers completed
* Disk read throughput
* Cache hit rate
* GPU transfer time
* Compute time
* Estimated completion progression
* Model shard health
* Current token
* Total tokens
* Queue depth
* SSD pressure
* Worker temperature and power
* Memory peak

AirLLM already contains profiling fields for disk loading, GPU loading and compression overhead, making this observability integration plausible.

# Updated recommendation

I would use:

```text
Ollama
  └── convenient local model management and API

llama.cpp
  └── portable high-control inference runtime

AirLLM
  └── oversized-model cold worker

Mojo
  └── experimental custom compute kernels

The-comune
  └── signed replication and provenance protocol

SparrowDB
  └── durable graph materialization and retrieval

Ratty / Bevy
  └── visible embodied control and observability plane
```

So my recommendation changes from “AirLLM is not relevant” to:

> **AirLLM is highly relevant—but as a deep, slow, local oracle rather than the nervous system of the agents.**

The most interesting future experiment would not begin with 671B. Start with a 30B–70B model that does not fit comfortably in your available unified memory, benchmark AirLLM against a quantized llama.cpp version, and record time-to-first-token, tokens per second, peak memory, disk reads and output quality. That will tell you whether the cold-worker role is practical on your specific Mac before committing hundreds of gigabytes to DeepSeek-V3.

[1]: https://huggingface.co/deepseek-ai/DeepSeek-V3 "deepseek-ai/DeepSeek-V3 · Hugging Face"
[2]: https://github.com/ggml-org/llama.cpp "GitHub - ggml-org/llama.cpp: LLM inference in C/C++ · GitHub"
