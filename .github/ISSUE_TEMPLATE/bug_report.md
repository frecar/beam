---
name: Bug report
about: Report a bug or unexpected behavior
title: ""
labels: bug
assignees: ""
---

Thanks for reporting a bug. Fill in what you can — even partial info helps.

**Describe the bug**
A clear description of what the bug is.

**To reproduce**
Steps to reproduce the behavior:
1. ...
2. ...

**Expected behavior**
What you expected to happen.

**Environment**
- OS: (e.g., Ubuntu 24.04)
- GPU: (e.g., NVIDIA RTX 3080, or "none/software encoding")
- Browser: (e.g., Chrome 130, Firefox 133)
- Beam version: (run `beam-server --version`)
- Install method: APT / source / tarball

**beam-doctor output**
Running `beam-doctor` captures most of the info above automatically — if you can run it, paste the output here.
```
(paste output of `beam-doctor` here)
```

**Logs**
```
(paste relevant output from `journalctl -u beam --since "10 minutes ago"`)
```

**Screenshots**
If applicable, add screenshots (especially the F9 performance overlay).
