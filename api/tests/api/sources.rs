use api::db::sources::SourceConfig;

use crate::test_app::{
    spawn_app, CreateSourceRequest, CreateSourceResponse, CreateTenantRequest,
    CreateTenantResponse, TestApp,
};

fn new_source_config() -> SourceConfig {
    SourceConfig::Postgres {
        host: "localhost".to_string(),
        port: 5432,
        name: "postgres".to_string(),
        username: "postgres".to_string(),
        password: Some("postgres".to_string()),
        slot_name: "slot".to_string(),
        publication: "publication".to_string(),
    }
}

async fn create_tenant(app: &TestApp) -> i64 {
    let tenant = CreateTenantRequest {
        name: "NewTenant".to_string(),
        supabase_project_ref: None,
    };
    let response = app.create_tenant(&tenant).await;
    let response: CreateTenantResponse = response
        .json()
        .await
        .expect("failed to deserialize response");
    response.id
}

#[tokio::test]
async fn source_can_be_created() {
    // Arrange
    let app = spawn_app().await;
    let tenant_id = create_tenant(&app).await;

    // Act
    let source = CreateSourceRequest {
        tenant_id,
        config: new_source_config(),
    };
    let response = app.create_source(&source).await;

    // Assert
    assert!(response.status().is_success());
    let response: CreateSourceResponse = response
        .json()
        .await
        .expect("failed to deserialize response");
    assert_eq!(response.id, 1);
}
