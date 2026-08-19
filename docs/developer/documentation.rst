Building the Documentation
==========================

This guide is for contributors who want to build and preview the Spur
documentation locally. The docs use `rocm-docs-core
<https://github.com/ROCm/rocm-docs-core>`_ as their base — a Sphinx extension and
theme — and are written in reStructuredText under ``docs/``.

There are two ways to build: a local Python virtual environment (fastest for
iterating), or the Docker image that CI uses (identical to the CI environment,
and also runs the linters).

Build with a Virtual Environment
--------------------------------

Prerequisites: Python 3.10+ and ``pip``. Run all commands from the repository
root.

1. Create a virtual environment (optional, but recommended):

   .. code-block:: bash

      python3 -m venv .venv/docs
      source .venv/docs/bin/activate

2. Install the pinned documentation toolchain:

   .. code-block:: bash

      pip install -r docs/sphinx/requirements.txt

3. Build the HTML:

   .. code-block:: bash

      python3 -m sphinx -b html -d _build/doctrees -D language=en ./docs docs/_build/html

4. Serve the built site locally on port 8000:

   .. code-block:: bash

      python3 -m http.server -d ./docs/_build/html/

5. Open http://localhost:8000 in a browser.

Auto-Rebuild While Editing
--------------------------

``sphinx-autobuild`` watches the ``docs`` directory and rebuilds on every change,
serving the result on port 8000:

.. code-block:: bash

   pip install sphinx-autobuild
   sphinx-autobuild -b html -d _build/doctrees -D language=en ./docs docs/_build/html \
       --ignore "docs/_build/*" --ignore "docs/sphinx/_toc.yml"

Build with Docker
-----------------

The ``docs/Dockerfile`` produces the same hermetic environment CI uses, with the
Sphinx toolchain and both linters (``markdownlint`` and ``pyspelling``)
pre-installed. This is the closest local match to the CI job.

1. Build the image (from the repository root):

   .. code-block:: bash

      docker build -f docs/Dockerfile -t spur-docs .

2. Build the HTML:

   .. code-block:: bash

      docker run --rm -v "$PWD:/work" -w /work spur-docs \
          sphinx-build -W --keep-going -b html docs docs/_build/html

3. Serve it locally:

   .. code-block:: bash

      python3 -m http.server -d ./docs/_build/html/

Linting
-------

CI gates documentation changes on a successful build plus clean spelling and
Markdown lint. Run the same checks locally with the Docker image:

.. code-block:: bash

   # Spelling (add new technical terms to .wordlist.txt at the repo root)
   docker run --rm -v "$PWD:/work" -w /work spur-docs pyspelling -c .spellcheck.yaml

   # Markdown style
   docker run --rm -v "$PWD:/work" -w /work spur-docs markdownlint-cli2 "docs/**/*.md"

Editing the Navigation
----------------------

Navigation is driven by an external table of contents, not by Sphinx ``toctree``
directives. To add, remove, or reorder a page, edit ``docs/sphinx/_toc.yml.in``
and make sure every page under ``docs/`` is listed exactly once. rocm-docs-core
generates ``docs/sphinx/_toc.yml`` from it at build time.

Troubleshooting
---------------

**A new navigation link does not appear.** The navigation menu is cached per
page, so previously built pages may not show a newly added link. Delete the
``docs/_build/`` directory and rebuild so the menu is regenerated for every page:

.. code-block:: bash

   rm -rf docs/_build

See Also
--------

- :doc:`contributing`
- :doc:`building`
