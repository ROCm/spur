# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Behavioural tests for the `quarantine` marker, run through ``pytester`` against
the real root conftest."""

from pathlib import Path

import pytest

pytest_plugins = ["pytester"]

_ROOT_CONFTEST = Path(__file__).resolve().parents[1] / "conftest.py"

REASON = "secondary control planes cannot rejoin without quorum"


@pytest.fixture
def quarantine_run(pytester):
    pytester.makeini(
        "[pytest]\n"
        "addopts = -p no:xdist\n"
        "markers =\n"
        "    quarantine: known-flaky test blocked on a product bug\n"
        "    k0s: k0s tests\n"
    )
    pytester.makeconftest(_ROOT_CONFTEST.read_text())
    return pytester


def test_quarantined_failing_test_reports_xfailed(quarantine_run):
    quarantine_run.makepyfile(f"""
        import pytest

        @pytest.mark.quarantine(reason="{REASON}")
        def test_broken():
            assert False, "product bug"
    """)
    result = quarantine_run.runpytest()
    result.assert_outcomes(xfailed=1)
    assert result.ret == 0


def test_quarantined_test_still_runs(quarantine_run):
    quarantine_run.makepyfile(f"""
        import pytest

        @pytest.mark.quarantine(reason="{REASON}")
        def test_runs():
            print("BODY-EXECUTED")
            assert False
    """)
    result = quarantine_run.runpytest("-s")
    result.stdout.fnmatch_lines(["*BODY-EXECUTED*"])
    result.assert_outcomes(xfailed=1)


def test_quarantined_passing_test_reports_xpassed_and_is_called_out(quarantine_run):
    quarantine_run.makepyfile(f"""
        import pytest

        @pytest.mark.quarantine(reason="{REASON}")
        def test_fixed():
            pass
    """)
    result = quarantine_run.runpytest()
    result.assert_outcomes(xpassed=1)
    assert result.ret == 0
    result.stdout.fnmatch_lines([
        "*Quarantined*",
        "XPASS*test_fixed*",
        f"*{REASON}*",
        "*consider removing the quarantine*",
    ])


def test_summary_lists_xfail_and_xpass_with_reasons(quarantine_run):
    quarantine_run.makepyfile("""
        import pytest

        @pytest.mark.quarantine(reason="still broken because A")
        def test_still_broken():
            assert False

        @pytest.mark.quarantine(reason="fixed because B")
        def test_now_fine():
            pass

        def test_unrelated():
            assert False
    """)
    result = quarantine_run.runpytest()
    result.assert_outcomes(failed=1, xfailed=1, xpassed=1)
    result.stdout.fnmatch_lines([
        "*Quarantined*",
        "XPASS*test_now_fine*",
        "*fixed because B*",
        "XFAIL*test_still_broken*",
        "*still broken because A*",
    ])
    summary = result.stdout.str().split("Quarantined", 1)[1].split("short test summary", 1)[0]
    assert "test_unrelated" not in summary


def test_no_summary_without_quarantined_tests(quarantine_run):
    quarantine_run.makepyfile("""
        def test_plain():
            pass
    """)
    result = quarantine_run.runpytest()
    assert "Quarantined" not in result.stdout.str()


def test_marker_without_reason_is_a_collection_error(quarantine_run):
    quarantine_run.makepyfile("""
        import pytest

        @pytest.mark.quarantine
        def test_bare():
            pass
    """)
    result = quarantine_run.runpytest()
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    result.stderr.fnmatch_lines(["*test_bare*quarantine*requires a non-empty reason*"])


@pytest.mark.parametrize("marker", [
    '@pytest.mark.quarantine("positional reason")',
    '@pytest.mark.quarantine(reason="")',
    '@pytest.mark.quarantine(reason="   ")',
])
def test_marker_with_unusable_reason_is_a_collection_error(quarantine_run, marker):
    quarantine_run.makepyfile(f"""
        import pytest

        {marker}
        def test_bad():
            pass
    """)
    result = quarantine_run.runpytest()
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    result.stderr.fnmatch_lines(["*test_bad*quarantine*requires a non-empty reason*"])


def test_quarantine_converts_every_shards_tests(quarantine_run):
    names = [f"test_{i}" for i in range(8)]
    quarantine_run.makepyfile("import pytest\n" + "".join(
        f'@pytest.mark.quarantine(reason="{REASON}")\ndef {n}():\n    assert False\n\n'
        for n in names
    ))
    xfailed = 0
    for shard_id in range(2):
        result = quarantine_run.runpytest("--shard-count=2", f"--shard-id={shard_id}")
        assert result.ret in (pytest.ExitCode.OK, pytest.ExitCode.NO_TESTS_COLLECTED)
        xfailed += result.parseoutcomes().get("xfailed", 0)
    assert xfailed == len(names)


@pytest.mark.parametrize("shard_id", [0, 1])
def test_invalid_marker_errors_in_every_shard(quarantine_run, shard_id):
    quarantine_run.makepyfile("""
        import pytest

        @pytest.mark.quarantine
        def test_bare():
            pass
    """)
    result = quarantine_run.runpytest("--shard-count=2", f"--shard-id={shard_id}")
    assert result.ret == pytest.ExitCode.USAGE_ERROR


def test_quarantine_keeps_k0s_last_ordering(quarantine_run):
    quarantine_run.makepyfile(f"""
        import pytest

        @pytest.mark.k0s
        @pytest.mark.quarantine(reason="{REASON}")
        def test_k0s_quarantined():
            assert False

        def test_plain():
            pass
    """)
    result = quarantine_run.runpytest("-v")
    out = result.stdout.str()
    assert out.index("test_plain") < out.index("test_k0s_quarantined")
    result.assert_outcomes(passed=1, xfailed=1)
