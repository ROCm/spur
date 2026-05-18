pub mod container;
pub mod jobs;
pub mod lifecycle;

use tokio::sync::OnceCell;

use crate::bare_metal::config::TestConfig;
use crate::bare_metal::fixture::BareMetalFixture;

static FIXTURE: OnceCell<BareMetalFixture> = OnceCell::const_new();

pub async fn fixture() -> &'static BareMetalFixture {
    FIXTURE
        .get_or_init(|| async {
            let config = TestConfig::from_env().expect("SPUR_TEST_BM_* config");
            BareMetalFixture::deploy(config)
                .await
                .expect("failed to deploy bare-metal cluster for single_node tests")
        })
        .await
}
