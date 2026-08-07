# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Quota projection: Spur accounts to native Kubernetes tenancy objects.

The operator's quota reconciler turns every Spur account into a Namespace, a
ResourceQuota built from the account's `grp_tres`, a LimitRange, and RBAC. It
server-side-applies with force on a 30s loop, so it both fills gaps and reverts
hand edits. These tests drive that loop against a real cluster.
"""

import time

import pytest
from kubernetes import client
from kubernetes.client.exceptions import ApiException

from k8s_cluster import assert_eventually, delete_namespace

QUOTA_NAME = "spur-account-quota"
LIMITS_NAME = "spur-account-defaults"
ROLE_NAME = "spur-account-editor"
BINDING_NAME = "spur-account-members"
MANAGED_BY = "spur-quota"

# The reconciler is level-triggered on a 30s interval, so anything it must
# converge on needs room for two full ticks.
RECONCILE_TIMEOUT = 100

ACCOUNTS = {
    "physics": "cpu=16,mem=32768,gres/gpu=8",
    "chem": "cpu=4",
}
MEMBERS = {"physics": ["alice", "bob"], "chem": ["carol"]}


def account_namespace(account: str) -> str:
    return f"spur-acct-{account}"


def rbac_api() -> client.RbacAuthorizationV1Api:
    return client.RbacAuthorizationV1Api()


def namespace_exists(name: str) -> bool:
    try:
        client.CoreV1Api().read_namespace(name)
        return True
    except ApiException as exc:
        if exc.status == 404:
            return False
        raise


@pytest.fixture(scope="class")
def seeded_quota_cluster(quota_cluster):
    """A quota cluster with accounts and members already projected.

    Seeding is class-scoped because the reconcile interval makes per-test
    seeding prohibitively slow.
    """
    if not quota_cluster.postgres_available():
        pytest.skip("quota projection requires accounting; postgres is not running")

    for account, grp_tres in ACCOUNTS.items():
        out = quota_cluster.spur_cli(
            ["sacctmgr", "add", "account", f"name={account}", f"grptres={grp_tres}"]
        )
        assert "Account added" in out, f"failed to create account {account}: {out}"
        for user in MEMBERS[account]:
            quota_cluster.spur_cli(
                ["sacctmgr", "add", "user", f"name={user}", f"account={account}"]
            )

    for account in ACCOUNTS:
        ns = account_namespace(account)
        assert_eventually(
            RECONCILE_TIMEOUT,
            5,
            f"quota reconciler never created namespace {ns}",
            lambda ns=ns: namespace_exists(ns),
        )

    yield quota_cluster

    for account in ACCOUNTS:
        delete_namespace(account_namespace(account))


class TestAccountProjection:
    def test_each_account_gets_its_own_namespace(self, seeded_quota_cluster):
        for account in ACCOUNTS:
            assert namespace_exists(account_namespace(account))

    def test_the_namespace_is_labelled_as_spur_managed(self, seeded_quota_cluster):
        """The label is the ownership contract — it is how the reconciler finds
        its objects and what makes drift correction safe."""
        ns = client.CoreV1Api().read_namespace(account_namespace("physics"))
        labels = ns.metadata.labels or {}
        assert labels.get("app.kubernetes.io/managed-by") == MANAGED_BY
        assert labels.get("spur.amd.com/account") == "physics"

    def test_grp_tres_becomes_resource_quota_hard_caps(self, seeded_quota_cluster):
        quota = client.CoreV1Api().read_namespaced_resource_quota(
            QUOTA_NAME, account_namespace("physics")
        )
        hard = quota.spec.hard
        assert hard["requests.cpu"] == "16"
        assert hard["requests.memory"] == "32768M"
        assert hard["requests.amd.com/gpu"] == "8"

    def test_limits_are_never_capped(self, seeded_quota_cluster):
        """A `limits.*` cap would force every pod in the namespace to declare a
        limit or fail admission, so the projection must stay requests-only."""
        quota = client.CoreV1Api().read_namespaced_resource_quota(
            QUOTA_NAME, account_namespace("physics")
        )
        assert "limits.cpu" not in quota.spec.hard
        assert "limits.memory" not in quota.spec.hard

    def test_unset_dimensions_are_left_uncapped(self, seeded_quota_cluster):
        """A 0 cap would block every pod, so an absent dimension must produce
        no key at all."""
        quota = client.CoreV1Api().read_namespaced_resource_quota(
            QUOTA_NAME, account_namespace("chem")
        )
        assert quota.spec.hard["requests.cpu"] == "4"
        assert "requests.memory" not in quota.spec.hard
        assert "requests.amd.com/gpu" not in quota.spec.hard

    def test_limit_range_defaults_requests_but_not_limits(self, seeded_quota_cluster):
        """Without a default request, a pod that omits requests would not count
        against the quota at all."""
        lr = client.CoreV1Api().read_namespaced_limit_range(
            LIMITS_NAME, account_namespace("physics")
        )
        item = lr.spec.limits[0]
        assert item.type == "Container"
        assert item.default_request["cpu"] == "100m"
        assert item.default_request["memory"] == "128M"
        assert item.default is None

    def test_the_account_role_grants_workload_management(self, seeded_quota_cluster):
        role = rbac_api().read_namespaced_role(
            ROLE_NAME, account_namespace("physics")
        )
        resources = {r for rule in role.rules for r in (rule.resources or [])}
        assert {"pods", "pods/log", "services", "configmaps"} <= resources

    def test_the_account_role_cannot_edit_its_own_quota(self, seeded_quota_cluster):
        """Self-editable quota would defeat the whole mechanism."""
        role = rbac_api().read_namespaced_role(
            ROLE_NAME, account_namespace("physics")
        )
        resources = {r for rule in role.rules for r in (rule.resources or [])}
        assert not resources & {
            "resourcequotas",
            "limitranges",
            "roles",
            "rolebindings",
        }

    def test_members_get_a_role_binding_subject_each(self, seeded_quota_cluster):
        binding = rbac_api().read_namespaced_role_binding(
            BINDING_NAME, account_namespace("physics")
        )
        subjects = {s.name for s in binding.subjects or []}
        assert subjects == {"spur-user-alice", "spur-user-bob"}
        assert all(s.kind == "ServiceAccount" for s in binding.subjects)
        assert binding.role_ref.name == ROLE_NAME

    def test_accounts_do_not_share_members(self, seeded_quota_cluster):
        binding = rbac_api().read_namespaced_role_binding(
            BINDING_NAME, account_namespace("chem")
        )
        subjects = {s.name for s in binding.subjects or []}
        assert subjects == {"spur-user-carol"}


class TestDriftCorrection:
    def test_a_hand_edited_quota_is_reverted(self, seeded_quota_cluster):
        """Server-side apply with force is what makes the Spur account, not the
        cluster admin, the source of truth."""
        core = client.CoreV1Api()
        ns = account_namespace("physics")
        core.patch_namespaced_resource_quota(
            QUOTA_NAME, ns, {"spec": {"hard": {"requests.cpu": "999"}}}
        )

        def restored() -> bool:
            quota = core.read_namespaced_resource_quota(QUOTA_NAME, ns)
            return quota.spec.hard.get("requests.cpu") == "16"

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "hand-edited cpu cap was not reverted", restored
        )

    def test_a_deleted_limit_range_is_recreated(self, seeded_quota_cluster):
        core = client.CoreV1Api()
        ns = account_namespace("physics")
        core.delete_namespaced_limit_range(LIMITS_NAME, ns)

        def recreated() -> bool:
            try:
                core.read_namespaced_limit_range(LIMITS_NAME, ns)
                return True
            except ApiException as exc:
                if exc.status == 404:
                    return False
                raise

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "deleted LimitRange was not recreated", recreated
        )

    def test_a_deleted_role_binding_is_recreated_with_its_subjects(
        self, seeded_quota_cluster
    ):
        ns = account_namespace("physics")
        rbac_api().delete_namespaced_role_binding(BINDING_NAME, ns)

        def restored() -> bool:
            try:
                binding = rbac_api().read_namespaced_role_binding(BINDING_NAME, ns)
            except ApiException as exc:
                if exc.status == 404:
                    return False
                raise
            return {s.name for s in binding.subjects or []} == {
                "spur-user-alice",
                "spur-user-bob",
            }

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "deleted RoleBinding was not restored", restored
        )

    def test_a_stripped_managed_by_label_comes_back(self, seeded_quota_cluster):
        core = client.CoreV1Api()
        ns = account_namespace("physics")
        core.patch_namespace(
            ns, {"metadata": {"labels": {"app.kubernetes.io/managed-by": "someone-else"}}}
        )

        def restored() -> bool:
            labels = core.read_namespace(ns).metadata.labels or {}
            return labels.get("app.kubernetes.io/managed-by") == MANAGED_BY

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "managed-by label was not restored", restored
        )


class TestAllocationChanges:
    def test_raising_an_allocation_raises_the_cap(self, seeded_quota_cluster):
        seeded_quota_cluster.spur_cli(
            ["sacctmgr", "modify", "account", "name=chem", "grptres=cpu=64,mem=1024"]
        )
        core = client.CoreV1Api()
        ns = account_namespace("chem")

        def raised() -> bool:
            hard = core.read_namespaced_resource_quota(QUOTA_NAME, ns).spec.hard
            return hard.get("requests.cpu") == "64" and hard.get("requests.memory") == "1024M"

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "cap did not follow the raised allocation", raised
        )

    def test_a_new_account_is_projected_without_a_restart(self, seeded_quota_cluster):
        """The loop is level-triggered, so an account added long after startup
        must still get picked up."""
        out = seeded_quota_cluster.spur_cli(
            ["sacctmgr", "add", "account", "name=bio", "grptres=cpu=2"]
        )
        assert "Account added" in out, out
        try:
            assert_eventually(
                RECONCILE_TIMEOUT,
                5,
                "a newly added account was never projected",
                lambda: namespace_exists(account_namespace("bio")),
            )
            quota = client.CoreV1Api().read_namespaced_resource_quota(
                QUOTA_NAME, account_namespace("bio")
            )
            assert quota.spec.hard["requests.cpu"] == "2"
        finally:
            delete_namespace(account_namespace("bio"))


class TestMalformedAllocation:
    def test_an_unparseable_allocation_is_rejected_at_submission(
        self, seeded_quota_cluster
    ):
        """Fail-closed starts at the accounting API: a grp_tres that could not
        be projected never reaches the database, so the reconciler cannot be
        tricked into leaving a namespace uncapped."""
        out = seeded_quota_cluster.spur_cli(
            ["sacctmgr", "add", "account", "name=broken", "grptres=cpu=notanumber"]
        )
        assert "Account added" not in out, out

    def test_a_rejected_account_gets_no_namespace(self, seeded_quota_cluster):
        seeded_quota_cluster.spur_cli(
            ["sacctmgr", "add", "account", "name=broken2", "grptres=cpu=!!"]
        )
        # Two reconcile ticks: long enough that a projection would have appeared.
        time.sleep(70)
        assert not namespace_exists(account_namespace("broken2"))

    def test_good_accounts_keep_reconciling(self, seeded_quota_cluster):
        """One bad account must not stall the loop for everyone else."""
        core = client.CoreV1Api()
        ns = account_namespace("physics")
        core.delete_namespaced_limit_range(LIMITS_NAME, ns)

        def recreated() -> bool:
            try:
                core.read_namespaced_limit_range(LIMITS_NAME, ns)
                return True
            except ApiException as exc:
                if exc.status == 404:
                    return False
                raise

        assert_eventually(
            RECONCILE_TIMEOUT, 5, "reconcile stalled after a bad account", recreated
        )
