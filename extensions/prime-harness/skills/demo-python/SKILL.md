---
name: demo-python
description: "Deterministic arithmetic helpers for M2 verification. Use when a task needs demo_python.add(a, b) — e.g. when asked to compute the demo addition."
---

# Demo Python Skill (M2 fixture)

`demo_python.add(a, b)` returns `a + b` computed in the kernel. It is
pre-imported into the IPython kernel; call it directly:

```python
demo_python.add(20, 22)  # -> 42
```
