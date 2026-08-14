#!/usr/bin/env python3
"""Validate the stable invariants and accounting in a rust-zero soak report."""

import argparse
import json
import sys
from pathlib import Path

WORKLOADS = {
    "rest_transport",
    "grpc_transport",
    "circuit_breaker_partial_failure",
    "overload_recovery",
    "large_discovery_snapshot",
    "queue_saturation",
}


def validate(report, minimum_seconds, minimum_cycles, max_rss_growth_kib):
    if report.get("schema_version") != 1 or report.get("framework") != "rust-zero":
        raise ValueError("not a rust-zero schema-v1 report")
    soak = report.get("soak")
    if not soak:
        raise ValueError("report does not contain a soak campaign")
    requested = soak["requested_seconds"]
    elapsed = soak["elapsed_seconds"]
    cycles = soak["cycles"]
    if requested < minimum_seconds or elapsed < requested:
        raise ValueError(
            f"campaign duration is insufficient: requested={requested}, elapsed={elapsed:.3f}"
        )
    if cycles < minimum_cycles:
        raise ValueError(f"campaign completed {cycles} cycles; {minimum_cycles} required")
    if soak["invariant_failures"] != 0:
        raise ValueError(f"campaign recorded {soak['invariant_failures']} invariant failures")
    summaries = soak["workloads"]
    if set(summaries) != WORKLOADS:
        raise ValueError(f"unexpected workload set: {sorted(summaries)}")
    for name, summary in summaries.items():
        if summary["cycles"] != cycles or summary["operations"] <= 0:
            raise ValueError(f"{name} has incomplete cycle or operation accounting")
        if summary["min_operations_per_second"] <= 0:
            raise ValueError(f"{name} did not record positive throughput")
    total_operations = sum(summary["operations"] for summary in summaries.values())
    if total_operations != soak["total_operations"]:
        raise ValueError("total operation count does not match workload summaries")
    expected_growth = max(0, soak["final_peak_rss_kib"] - soak["initial_peak_rss_kib"])
    if soak["peak_rss_growth_kib"] != expected_growth:
        raise ValueError("peak RSS growth accounting is inconsistent")
    if max_rss_growth_kib is not None and expected_growth > max_rss_growth_kib:
        raise ValueError(
            f"peak RSS grew by {expected_growth} KiB; limit is {max_rss_growth_kib} KiB"
        )
    return soak


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report")
    parser.add_argument("--minimum-seconds", type=int, default=1)
    parser.add_argument("--minimum-cycles", type=int, default=2)
    parser.add_argument("--max-rss-growth-kib", type=int)
    args = parser.parse_args()
    try:
        with Path(args.report).open(encoding="utf-8") as stream:
            report = json.load(stream)
        soak = validate(
            report,
            args.minimum_seconds,
            args.minimum_cycles,
            args.max_rss_growth_kib,
        )
        print(
            f"soak report valid: {soak['cycles']} cycles, "
            f"{soak['total_operations']} operations, "
            f"{soak['peak_rss_growth_kib']} KiB peak RSS growth"
        )
        return 0
    except (KeyError, TypeError, ValueError, OSError, json.JSONDecodeError) as error:
        print(f"soak validation failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
