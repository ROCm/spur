Contributing
============

Getting Started
---------------

1. Fork the repository on GitHub
2. Clone your fork and add the upstream remote:

.. code-block:: bash

   git clone https://github.com/<your-username>/spur.git
   cd spur
   git remote add upstream https://github.com/ROCm/spur.git

3. Follow the :doc:`building` guide to build the project and run tests. Get familiar with the build and test workflow before making changes.

4. Before starting work, sync with upstream:

.. code-block:: bash

   git fetch upstream
   git rebase upstream/main

PR Title Format
---------------

PRs are squash-merged, so the PR title becomes the commit message on ``main``. Use conventional commit format for your PR title: ``<type>(<scope>): <description>``

**type** — one of: ``feat``, ``fix``, ``refactor``, ``test``, ``docs``, ``style``, ``perf``, ``build``, ``ci``, ``chore``, ``revert``

**scope** — the crate name (e.g. ``spur-cli``, ``spurctld``, ``spur-core``, ``spur-k8s``). If no single crate applies, use a concise scope reflecting the area of change (``proto``, ``examples``, ``config``).

**description** — imperative mood, lowercase, no trailing period. Summarize the user-visible change, not the implementation detail.

Append ``!`` after the type or scope (e.g. ``fix(spur-core)!: ...``) to mark a breaking change.

Example:

.. code-block:: text

   feat(spur-k8s): add support for GPU resource limits in SpurJob spec

Only ``feat``, ``fix``, and ``perf`` appear in the release notes. Non-user-facing work (CI, tooling, infra, tests) must use a type that stays out of them, such as ``ci``, ``build``, ``test``, or ``chore``. CI rejects titles that pair ``feat``, ``fix``, or ``perf`` with any of these scopes: ``ci``, ``cd``, ``build``, ``release``, ``infra``, ``e2e``, ``docs``, ``test``, ``tests``, ``style``, ``chore``, ``revert``. For example, use ``ci(e2e): fix flaky test`` instead of ``fix(e2e): fix flaky test``.

License Headers
---------------

All new source files (``.rs``, ``.proto``, ``.py``, ``.sh``, and ``Dockerfile``) must include this header at the top:

.. code-block:: text

   // Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
   // SPDX-License-Identifier: Apache-2.0

Use ``#`` instead of ``//`` for Python, shell scripts, and ``Dockerfile``. The header must appear within the first five lines, so a shebang line before it is fine.

Pre-Commit Hook
---------------

The repo includes a pre-commit hook in ``.githooks/``. Opt in by pointing git to the hooks directory:

.. code-block:: bash

   git config core.hooksPath .githooks

The hook runs two checks:

- ``cargo fmt --all --check`` (code formatting)
- ``scripts/check-license-headers.sh`` on staged source files (SPDX license headers)

If the hook fails, the commit is aborted. Fix the issue (for formatting, run ``cargo fmt --all``), stage the changes, and commit again. You can bypass the hook with ``git commit --no-verify``, but CI still enforces both checks.

PR Process
----------

1. Create a feature branch from ``main`` on your fork
2. Make your changes in logical commits following the commit style above
3. Ensure all pre-commit hook checks pass
4. Push to your fork and open a PR against ``ROCm/spur:main``

.. note::

   All PRs must pass CI checks to be eligible for merging. If you see a CI test failing that doesn't look related to your changes, tag a maintainer in the PR for help.

Review Process
--------------

Maintainers look for clear code structure, adequate test coverage, and long-term maintainability. In particular:

- New functionality has corresponding tests
- Features are complete end-to-end within a single PR. Avoid partial implementations that leave unused code or require follow-up PRs to become functional
- Commits are logically separated and follow the commit style
- The PR description explains what changed and why
