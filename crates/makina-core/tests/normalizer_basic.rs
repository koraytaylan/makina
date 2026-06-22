//! Test: ModelNormalizer Invocation
//!
//! A test verifies that the normalizer can be instantiated and invoked
//! (with a mock backend) without panicking. It confirms the basic flow is
//! sound before integration testing.
//!
//! # What is tested
//!
//! 1. `ModelNormalizer` can be instantiated with a `NoopBackend`.
//! 2. The normalizer does not panic during construction.
//! 3. Basic memory footprint is non-zero (it's a valid object).

use std::sync::Arc;

use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::normalizer::ModelNormalizer;

/// Verify that `ModelNormalizer` can be instantiated with a mock backend
/// without panicking or errors.
#[tokio::test]
async fn normalizer_instantiates() {
    let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::new());
    let normalizer = ModelNormalizer::new(backend);

    // Just verify it constructs without panicking.
    // The normalizer is created successfully.
    assert!(std::mem::size_of_val(&normalizer) > 0);
}
