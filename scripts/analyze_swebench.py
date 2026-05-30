#!/usr/bin/env python3
"""SWE-bench搜索效率对比分析脚本"""
import json, sys

def load_results(path):
    with open(path) as f:
        return json.load(f)

def compare(baseline_file, experiment_file):
    bl = load_results(baseline_file)
    ex = load_results(experiment_file)
    
    print("=== SWE-bench Search Efficiency Comparison ===")
    print(f"Baseline:      {baseline_file}")
    print(f"Experiment:    {experiment_file}")
    print()
    
    # Resolution rate
    bl_resolved = sum(1 for r in bl.values() if r.get("resolved", False))
    ex_resolved = sum(1 for r in ex.values() if r.get("resolved", False))
    total = len(bl)
    
    print("--- Resolution Rate ---")
    print(f"  Baseline:      {bl_resolved}/{total} ({bl_resolved/total*100:.1f}%)")
    print(f"  Experiment:    {ex_resolved}/{total} ({ex_resolved/total*100:.1f}%)")
    diff = ex_resolved - bl_resolved
    print(f"  Difference:    {'+' if diff >= 0 else ''}{diff} tasks")
    print()
    
    # Per-task comparison
    print("--- Per-task Detail ---")
    print(f"  {'Instance':<40} {'Baseline':<12} {'Experiment':<12} {'Delta'}")
    
    for instance_id in sorted(bl.keys()):
        b = bl[instance_id]
        e = ex.get(instance_id, {})
        b_time = b.get("elapsed", b.get("time", "N/A"))
        e_time = e.get("elapsed", e.get("time", "N/A"))
        b_res = "✅" if b.get("resolved") else "❌"
        e_res = "✅" if e.get("resolved") else "❌"
        
        change = ""
        if b.get("resolved") and not e.get("resolved"):
            change = "⚠ REGRESSION"
        elif not b.get("resolved") and e.get("resolved"):
            change = "🟢 NEW"
        
        print(f"  {instance_id:<40} {b_res} {str(b_time):<10} {e_res} {str(e_time):<10} {change}")

if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("Usage: python analyze_swebench.py <baseline.json> <experiment.json>")
        sys.exit(1)
    compare(sys.argv[1], sys.argv[2])
