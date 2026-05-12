pub mod cross_namespace;
pub mod spurjob;

use super::fixture::{ClusterFixture, FixtureConfig};
use tokio::sync::OnceCell;

static FIXTURE: OnceCell<ClusterFixture> = OnceCell::const_new();

pub async fn fixture() -> &'static ClusterFixture {
    FIXTURE
        .get_or_init(|| async {
            let suite = super::suite_context().await;
            ClusterFixture::deploy(suite, FixtureConfig::single_node())
                .await
                .expect("failed to deploy single-node Spur for k8s tests")
        })
        .await
}
