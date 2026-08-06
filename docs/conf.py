# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Configuration file for the Sphinx documentation builder.
# Spur uses rocm-docs-core: enabling the "rocm_docs" extension and the
# "rocm_docs_theme" theme pulls in and pre-configures the whole ROCm Sphinx
# toolchain (MyST, sphinx-external-toc, sphinx-design, copybutton, ...).
import os
import re

# Read the Docs sets these; used for the canonical URL and version banner.
html_baseurl = os.environ.get("READTHEDOCS_CANONICAL_URL", "")
html_context = {}
if os.environ.get("READTHEDOCS", "") == "True":
    html_context["READTHEDOCS"] = True


def _workspace_version(default="0.0.0"):
    # Track the crate version from the workspace Cargo.toml so the docs and the
    # code never drift. Falls back to a placeholder if the file moves.
    cargo = os.path.join(os.path.dirname(__file__), "..", "Cargo.toml")
    try:
        with open(cargo, encoding="utf-8") as handle:
            in_workspace_package = False
            for line in handle:
                stripped = line.strip()
                if stripped.startswith("["):
                    in_workspace_package = stripped == "[workspace.package]"
                    continue
                if in_workspace_package:
                    match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
                    if match:
                        return match.group(1)
    except OSError:
        pass
    return default


project = "Spur"
version = _workspace_version()
release = version
author = "Advanced Micro Devices, Inc."
copyright = "Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved."
html_title = f"{project} {version}"

# Keep the build hermetic: don't fetch external-project intersphinx mappings from
# the GitHub API (unauthenticated calls rate-limit and stall the build). Spur has
# no cross-project references yet.
external_projects_remote_repository = ""
external_projects = []

# Required rocm-docs-core settings.
extensions = ["rocm_docs"]
html_theme = "rocm_docs_theme"
html_theme_options = {
    "flavor": "instinct",
    "link_main_doc": True,
    "repository_url": "https://github.com/ROCm/spur",
    "repository_branch": "main",
    "path_to_docs": "docs",
}

# Navigation comes from the external table of contents, not toctree directives.
# rocm-docs-core generates ./sphinx/_toc.yml from ./sphinx/_toc.yml.in at build time.
external_toc_path = "./sphinx/_toc.yml"

exclude_patterns = ["_build", ".venv", "sphinx/_toc.yml.in"]


def setup(app):
    # Hermetic build (external_projects = []): rocm-docs-core's registry lookup of
    # "Spur" can't resolve, so drop only that message to keep -W meaningful.
    import logging
    from sphinx.util import logging as sphinx_logging

    class _DropProjectsLookup(logging.Filter):
        def filter(self, record):
            return "not found in projects" not in record.getMessage()

    sphinx_logging.getLogger("rocm_docs.projects").logger.addFilter(_DropProjectsLookup())
