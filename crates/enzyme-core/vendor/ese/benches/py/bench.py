import inspect
import json
import sys
import time

import numpy as np
from datasets import load_dataset

from model_loader import load_model

bench_time = 10
discard_percent = 10
model_name = sys.argv[1]
print(f"Benchmarking GooAQ QPS for {model_name}")


model = load_model(model_name)
ds = load_dataset("sentence-transformers/gooaq", split="train")
queries = ds["question"]
n = len(queries)
model.encode(queries[0])  # allow for any initialization to run

step = 32
try:
    model_batch_size = inspect.signature(model.encode).parameters["batch_size"].default
    step = model_batch_size
    print(f"\t/!\\ using model default batch size of {model_batch_size}")
finally:
    pass

# Single-query QPS
offset = 0
single_times = []
single_start = time.perf_counter()
while True:
    if offset + step <= n:
        batch = queries[offset : offset + step]
    else:
        batch = queries[offset:] + queries[: (offset + step) - n]

    for q in batch:
        s = time.perf_counter()
        e = model.encode(q)
        single_times.append(time.perf_counter() - s)
    offset += step
    offset %= n

    single_total = sum(single_times)
    single_count = len(single_times)
    single_qps = single_count / single_total
    wall_elapsed = time.perf_counter() - single_start

    if wall_elapsed >= bench_time:
        break

    print(
        f"Single live QPS: {single_qps:4.2f} ({single_count}, {single_total:.2f}s, {bench_time - wall_elapsed:.2f}s)",
        end="\r",
    )

# discard early samples as warmups
single_discard = len(single_times) // discard_percent
single_times = single_times[single_discard:]
single_total = sum(single_times)
single_count = len(single_times)
single_qps = single_count / single_total

print(
    f"Single-query QPS: {single_qps:.2f} ({single_count} in {single_total:.6f}s)"
    "                     "
)

# Batched QPS
offset = 0
batch_times = []
batch_start = time.perf_counter()
while True:
    if offset + step <= n:
        batch = queries[offset : offset + step]
    else:
        batch = queries[offset:] + queries[: (offset + step) - n]

    s = time.perf_counter()
    e = model.encode(batch)
    batch_times.append(time.perf_counter() - s)
    offset += step
    offset %= n

    batch_total = sum(batch_times)
    batch_count = (len(batch_times)) * step
    batch_qps = batch_count / batch_total
    wall_elapsed = time.perf_counter() - batch_start

    if wall_elapsed >= bench_time:
        break

    print(
        f"Batch live QPS: {batch_qps:4.2f} ({batch_count}, {batch_total:.2f}s, {bench_time - wall_elapsed:.2f}s)",
        end="\r",
    )

# discard warmups
batch_discard = len(batch_times) // discard_percent
batch_times = batch_times[batch_discard:]
batch_total = sum(batch_times)
batch_count = len(batch_times) * step
batch_qps = batch_count / batch_total

print(
    f"Batched QPS: {batch_qps:.2f} ({batch_count} in {batch_total:.6f}s)"
    "                     "
)


results = {
    "model": model_name,
    "bench_time_target": bench_time,
    "step": step,
    "single_query": {
        "count": single_count,
        "total_time": single_total,
        "qps": single_qps,
        "discard": single_discard,
        "latency_mean": np.mean(single_times),
        "latency_median": np.median(single_times),
        "latency_std": np.std(single_times),
        "latency_p50": np.percentile(single_times, 50),
        "latency_p95": np.percentile(single_times, 95),
        "latency_p99": np.percentile(single_times, 99),
        "latency_min": np.min(single_times),
        "latency_max": np.max(single_times),
    },
    "batched": {
        "count": batch_count,
        "total_time": batch_total,
        "qps": batch_qps,
        "discard": batch_discard,
        "batch_latency_mean": np.mean(batch_times),
        "batch_latency_median": np.median(batch_times),
        "batch_latency_std": np.std(batch_times),
        "batch_latency_p50": np.percentile(batch_times, 50),
        "batch_latency_p95": np.percentile(batch_times, 95),
        "batch_latency_p99": np.percentile(batch_times, 99),
        "batch_latency_min": np.min(batch_times),
        "batch_latency_max": np.max(batch_times),
        "per_query_latency_mean": np.mean(batch_times) / step,
    },
}

out_path = f"benchmarks/{model_name.replace('/', '_')}_bench.json"


class NumpyEncoder(json.JSONEncoder):
    def default(self, obj):
        if isinstance(obj, np.floating):
            return float(obj)
        if isinstance(obj, np.integer):
            return int(obj)
        return super().default(obj)


with open(out_path, "w") as f:
    json.dump(results, f, indent=2, cls=NumpyEncoder)

print(f"Results saved to {out_path}")
