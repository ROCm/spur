# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for account-level DefaultQOS and AllowedQOS.

Proves that QOS can be set on accounts (not just user associations) and
that the scheduler resolves QOS through the account-level fallback chain:
  user-association default → account default → cluster default.

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).

Closes ROCm/spur#1072.
"""

import time

import pytest

from cluster import parse_job_id, wait_job_state


_BASE_CONFIG = {
    "partitions": [
        {
            "name": "default",
            "state": "UP",
            "default": True,
            "nodes": "ALL",
            "max_time": "24:00:00",
            "default_time": "10:00",
        }
    ],
    "auth": {"plugin": "none", "allow_root_jobs": True},
}

_CACHE_SETTLE = 15


class TestAccountDefaultQos:
    """sacctmgr add/modify account with defaultqos= and qos= fields."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_add_account_with_defaultqos(self, accounting_cluster):
        """sacctmgr add account accepts defaultqos= and qos= fields."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=acctqos1"])
        c.sacctmgr([
            "add", "account", "name=acctdef1",
            "defaultqos=acctqos1", "qos=acctqos1",
        ])

        out = c.sacctmgr(["show", "account", "format=Account,DefQOS,QOS", "-n"])
        assert "acctdef1" in out
        assert "acctqos1" in out

    def test_modify_account_defaultqos(self, accounting_cluster):
        """sacctmgr modify account set defaultqos= updates the stored value."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=modqos1"])
        c.sacctmgr(["add", "qos", "name=modqos2"])
        c.sacctmgr([
            "add", "account", "name=acctmod1",
            "defaultqos=modqos1", "qos=modqos1,modqos2",
        ])

        out = c.sacctmgr(["show", "account", "format=Account,DefQOS", "-n", "-P"])
        assert "acctmod1|modqos1" in out

        c.sacctmgr([
            "modify", "account", "name=acctmod1",
            "set", "defaultqos=modqos2",
        ])

        out = c.sacctmgr(["show", "account", "format=Account,DefQOS", "-n", "-P"])
        assert "acctmod1|modqos2" in out

    def test_modify_account_defaultqos_not_in_allowlist_rejected(self, accounting_cluster):
        """defaultqos must be in the qos allow-list when both are set."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=allowed1"])
        c.sacctmgr(["add", "qos", "name=notallowed1"])
        try:
            c.sacctmgr([
                "add", "account", "name=acctbad1",
                "defaultqos=notallowed1", "qos=allowed1",
            ])
            pytest.fail("expected sacctmgr to reject defaultqos not in qos")
        except Exception as e:
            assert "must be included in qos" in str(e).lower() or "error" in str(e).lower()

    def test_add_account_unknown_defaultqos_rejected(self, accounting_cluster):
        """The server rejects an account defaultqos= naming a missing QOS."""
        c = accounting_cluster
        out = c.cli_allow_fail([
            "sacctmgr", "add", "account", "name=acctbadqos1", "defaultqos=nosuchqos1",
        ])
        assert "does not exist" in out, out
        out = c.cli_allow_fail([
            "sacctmgr", "add", "account", "name=acctbadqos2", "qos=nosuchqos2",
        ])
        assert "does not exist" in out, out
        listed = c.sacctmgr(["show", "account", "format=Account", "-n", "-P"])
        assert "acctbadqos" not in listed, listed

    def test_modify_account_set_default_and_clear_allowlist(self, accounting_cluster):
        """modify account set defaultqos=X qos= sets the default and clears the list."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=clrqos1"])
        c.sacctmgr(["add", "qos", "name=clrqos2"])
        c.sacctmgr([
            "add", "account", "name=acctclr1",
            "defaultqos=clrqos1", "qos=clrqos1",
        ])
        c.sacctmgr([
            "modify", "account", "name=acctclr1",
            "set", "defaultqos=clrqos2", "qos=",
        ])

        out = c.sacctmgr(["show", "account", "format=Account,DefQOS,QOS", "-n", "-P"])
        assert "acctclr1|clrqos2|" in out, out


class TestAccountQosResolution:
    """The scheduler must fall back to the account's DefaultQOS when the
    user-association has no default of its own."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_job_inherits_account_default_qos(self, accounting_cluster):
        """A job submitted without --qos inherits the account's DefaultQOS
        when the user association has no default set."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=acctdefault"])
        c.sacctmgr([
            "add", "account", "name=teamqos",
            "defaultqos=acctdefault", "qos=acctdefault",
        ])
        c.sacctmgr([
            "add", "user", f"name={c.nodes[0].user}", "account=teamqos",
        ])
        time.sleep(_CACHE_SETTLE)

        script = c.write_file("acctqos-inherit.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "acctqos-inherit", "-A", "teamqos", script])
        )
        assert job_id is not None
        wait_job_state(c, job_id, "R", timeout=30)

        show = c.scontrol("show", "job", str(job_id))
        assert "QOS=acctdefault" in show, (
            f"job should inherit account's DefaultQOS 'acctdefault':\n{show}"
        )
        c.cli_allow_fail(["scancel", str(job_id)])

    def test_user_default_qos_overrides_account_default(self, accounting_cluster):
        """A user-association DefaultQOS takes precedence over the account's."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=acctlevel"])
        c.sacctmgr(["add", "qos", "name=userlevel"])
        c.sacctmgr([
            "add", "account", "name=teamover",
            "defaultqos=acctlevel", "qos=acctlevel,userlevel",
        ])
        c.sacctmgr([
            "add", "user", f"name={c.nodes[0].user}", "account=teamover",
            "defaultqos=userlevel", "qos=acctlevel,userlevel",
        ])
        time.sleep(_CACHE_SETTLE)

        script = c.write_file("userqos-override.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "userqos-override", "-A", "teamover", script])
        )
        assert job_id is not None
        wait_job_state(c, job_id, "R", timeout=30)

        show = c.scontrol("show", "job", str(job_id))
        assert "QOS=userlevel" in show, (
            f"user-level default should override account default:\n{show}"
        )
        c.cli_allow_fail(["scancel", str(job_id)])

    def test_account_default_outside_user_allowlist_not_granted(self, accounting_cluster):
        """An account default the user's allow-list excludes is neither
        assigned nor authorized for that user."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=usernormal"])
        c.sacctmgr(["add", "qos", "name=accthigh"])
        c.sacctmgr([
            "add", "account", "name=teamscope", "defaultqos=accthigh",
        ])
        c.sacctmgr([
            "add", "user", f"name={c.nodes[0].user}", "account=teamscope",
            "qos=usernormal",
        ])
        time.sleep(_CACHE_SETTLE)

        script = c.write_file("acctqos-scope.sh", "#!/bin/bash\nsleep 5\n")
        rc, out = c.sbatch_with_exit([
            "-J", "acctqos-scope", "-A", "teamscope", "-q", "accthigh", script,
        ])
        assert rc != 0 and "not permitted" in out, out

        job_id = parse_job_id(
            c.sbatch(["-J", "acctqos-scope", "-A", "teamscope", script])
        )
        assert job_id is not None
        show = c.scontrol("show", "job", str(job_id))
        assert "QOS=accthigh" not in show, (
            f"account default outside the user's allow-list must not be inherited:\n{show}"
        )
        c.cli_allow_fail(["scancel", str(job_id)])


class TestAccountAllowedQos:
    """Account-level AllowedQOS gates which QOS a user can submit with."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_account_allowed_qos_permits_listed(self, accounting_cluster):
        """A user can submit with a QOS listed in the account's AllowedQOS."""
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=permitted1"])
        c.sacctmgr([
            "add", "account", "name=teamallow",
            "qos=permitted1", "defaultqos=permitted1",
        ])
        c.sacctmgr([
            "add", "user", f"name={c.nodes[0].user}", "account=teamallow",
        ])
        time.sleep(_CACHE_SETTLE)

        script = c.write_file("allow-listed.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            c.sbatch([
                "-J", "allow-listed", "-A", "teamallow",
                "-q", "permitted1", script,
            ])
        )
        assert job_id is not None
        wait_job_state(c, job_id, "R", timeout=30)

        show = c.scontrol("show", "job", str(job_id))
        assert "QOS=permitted1" in show, (
            f"QOS in account allow-list should be accepted:\n{show}"
        )
        c.cli_allow_fail(["scancel", str(job_id)])


class TestShowAccountQosFields:
    """sacctmgr show account displays DefQOS and QOS columns."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_show_account_displays_qos_columns(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=showqos1"])
        c.sacctmgr(["add", "qos", "name=showqos2"])
        c.sacctmgr([
            "add", "account", "name=showacct",
            "defaultqos=showqos1", "qos=showqos1,showqos2",
        ])

        out = c.sacctmgr([
            "show", "account", "format=Account,DefQOS,QOS", "-P",
        ])
        lines = [line for line in out.strip().split("\n") if "showacct" in line]
        assert len(lines) == 1, f"expected one showacct row:\n{out}"
        fields = lines[0].split("|")
        assert fields[0] == "showacct"
        assert fields[1] == "showqos1"
        assert set(fields[2].split(",")) == {"showqos1", "showqos2"}
