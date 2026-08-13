#!/usr/bin/env python3
"""Summarize repeated benchmark reports and enforce median regression limits."""

import argparse
import json
import statistics
import sys
from pathlib import Path

METRICS = ("operations_per_second", "p95_us", "p99_us", "allocations", "allocated_bytes")


def load(path):
    with Path(path).open(encoding="utf-8") as stream:
        return json.load(stream)


def summarize(paths, minimum_samples):
    reports = [load(path) for path in paths]
    groups = {}
    for report in reports:
        if report.get("schema_version") != 1:
            raise ValueError("only report schema version 1 is supported")
        framework = report.get("framework")
        if not framework:
            raise ValueError("report is missing framework identity")
        groups.setdefault(framework, []).append(report)

    result = {"schema_version": 1, "minimum_samples": minimum_samples, "frameworks": {}}
    for framework, samples in sorted(groups.items()):
        if len(samples) < minimum_samples:
            raise ValueError(f"{framework} has {len(samples)} samples; {minimum_samples} required")
        configs = {json.dumps(sample["config"], sort_keys=True) for sample in samples}
        targets = {sample["target"] for sample in samples}
        versions = {sample["framework_version"] for sample in samples}
        if len(configs) != 1 or len(targets) != 1 or len(versions) != 1:
            raise ValueError(f"{framework} samples do not share config, target, and version")
        workloads = {}
        names = {item["name"] for item in samples[0]["workloads"]}
        for sample in samples[1:]:
            if {item["name"] for item in sample["workloads"]} != names:
                raise ValueError(f"{framework} workload sets differ")
        for name in sorted(names):
            measurements = [next(item for item in sample["workloads"] if item["name"] == name) for sample in samples]
            operations = {item["operations"] for item in measurements}
            counter_names = {key for item in measurements for key in item["counters"]}
            if len(operations) != 1 or any(set(item["counters"]) != counter_names for item in measurements):
                raise ValueError(f"{framework}/{name} operations or counter sets differ")
            workloads[name] = {
                **{metric: statistics.median(item[metric] for item in measurements) for metric in METRICS},
                "operations": operations.pop(),
                "counters": {
                    key: statistics.median(item["counters"][key] for item in measurements)
                    for key in sorted(counter_names)
                },
            }
        result["frameworks"][framework] = {
            "framework_version": samples[0]["framework_version"],
            "target": samples[0]["target"],
            "sample_count": len(samples),
            "config": samples[0]["config"],
            "workloads": workloads,
        }
    return result


def check(baseline, candidate, thresholds):
    failures = []
    defaults = thresholds["default"]
    overrides = thresholds.get("workloads", {})
    for framework, expected in baseline["frameworks"].items():
        actual = candidate["frameworks"].get(framework)
        if actual is None:
            failures.append(f"missing framework {framework}")
            continue
        if actual["target"] != expected["target"]:
            failures.append(f"{framework}: target changed from {expected['target']} to {actual['target']}")
            continue
        if actual["config"] != expected["config"]:
            failures.append(f"{framework}: workload configuration changed")
            continue
        for workload, expected_metrics in expected["workloads"].items():
            if workload not in actual["workloads"]:
                failures.append(f"{framework}/{workload}: missing workload")
                continue
            rules = {**defaults, **overrides.get(workload, {})}
            for metric, rule in rules.items():
                old = expected_metrics[metric]
                new = actual["workloads"][workload][metric]
                limit = rule["max_regression_percent"] / 100.0
                regressed = new < old * (1.0 - limit) if rule["direction"] == "higher" else new > old * (1.0 + limit)
                if regressed:
                    failures.append(f"{framework}/{workload}/{metric}: baseline={old:.3f}, candidate={new:.3f}, limit={limit:.0%}")
    return failures


def main():
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    summary = subparsers.add_parser("summarize")
    summary.add_argument("reports", nargs="+")
    summary.add_argument("--minimum-samples", type=int, default=5)
    summary.add_argument("--output", required=True)
    verify = subparsers.add_parser("check")
    verify.add_argument("--baseline", required=True)
    verify.add_argument("--candidate", required=True)
    verify.add_argument("--thresholds", required=True)
    args = parser.parse_args()

    try:
        if args.command == "summarize":
            output = summarize(args.reports, args.minimum_samples)
            Path(args.output).write_text(json.dumps(output, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            return 0
        failures = check(load(args.baseline), load(args.candidate), load(args.thresholds))
        if failures:
            print("benchmark regressions:", file=sys.stderr)
            for failure in failures:
                print(f"- {failure}", file=sys.stderr)
            return 1
        print("benchmark medians are within regression thresholds")
        return 0
    except (KeyError, ValueError, OSError, json.JSONDecodeError) as error:
        print(f"benchmark comparison failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
