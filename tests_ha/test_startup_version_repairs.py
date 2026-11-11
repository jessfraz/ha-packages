"""A failed setup still has a usable, entry-owned native version Repair."""

from homeassistant.components.update import UpdateEntityFeature
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant, ServiceCall
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.setup import async_setup_component
from pytest_homeassistant.common import MockConfigEntry
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.packages._app_kit.version import LOADED_VERSION
from custom_components.packages.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)
from custom_components.packages.diagnostics import async_get_config_entry_diagnostics
from custom_components.packages.repairs import VERSION_REPAIR

from .conftest import Backend


async def test_failed_startup_version_repair_opens_in_native_repairs(
    hass: HomeAssistant, backend: Backend, hass_client: ClientSessionGenerator
) -> None:
    backend.version = "0.0.1"
    backend.responses[("GET", "/v1/delivery-estimates")] = (404, b"{}", {})
    registry = er.async_get(hass)
    app = registry.async_get_or_create(
        "update", "hassio", "fixture_packages_version_latest"
    )
    companion = registry.async_get_or_create(
        "update", "marketplace", VERSION_REPAIR.repository_id
    )
    for entity_id, version in (
        (app.entity_id, backend.version),
        (companion.entity_id, LOADED_VERSION),
    ):
        hass.states.async_set(
            entity_id,
            "on",
            {
                "supported_features": int(
                    UpdateEntityFeature.INSTALL | UpdateEntityFeature.SPECIFIC_VERSION
                ),
                "installed_version": version,
                "latest_version": LOADED_VERSION,
                "in_progress": False,
            },
        )
    writes: list[str] = []

    async def refresh(_call: ServiceCall) -> None:
        return

    async def forbidden(call: ServiceCall) -> None:
        writes.append(call.service)
        raise AssertionError("No lifecycle writes before confirmation")

    hass.services.async_register("homeassistant", "update_entity", refresh)
    hass.services.async_register("homeassistant", "restart", forbidden)
    hass.services.async_register("update", "install", forbidden)
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "test-integration-token",
            CONF_ADDON_SLUG: "fixture_packages",
        },
    )
    entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(entry.entry_id)
    assert entry.state is ConfigEntryState.SETUP_RETRY
    issues = [
        (issue_id, issue)
        for (domain, issue_id), issue in ir.async_get(hass).issues.items()
        if domain == DOMAIN and issue_id.endswith("version_mismatch")
    ]
    assert len(issues) == 1
    issue_id, issue = issues[0]

    assert await async_setup_component(hass, "repairs", {})
    client = await hass_client()
    response = await client.post(
        "/api/repairs/issues/fix", json={"handler": DOMAIN, "issue_id": issue_id}
    )
    assert response.status == 200
    flow = await response.json()
    assert flow["type"] == "menu"
    assert flow["menu_options"] == ["update_and_restart"]
    assert issue_id == f"{entry.entry_id}_version_mismatch"
    assert issue.is_fixable and issue.is_persistent
    assert issue.config_entry_id == entry.entry_id
    assert issue.translation_key == "version_mismatch"
    assert ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch") is None
    assert ir.async_get(hass).async_get_issue(DOMAIN, issue_id) is issue
    assert all(request.method == "GET" for request in backend.requests)
    assert writes == []
    requests = len(backend.requests)
    diagnostics = await async_get_config_entry_diagnostics(hass, entry)
    assert diagnostics["repairs"]["version_mismatch"]
    assert not diagnostics["coordinator"]["available"]
    assert len(backend.requests) == requests
    response = await client.delete(f"/api/repairs/issues/fix/{flow['flow_id']}")
    assert response.status == 200
    assert writes == []
    assert ir.async_get(hass).async_get_issue(DOMAIN, issue_id) is issue

    backend.version = LOADED_VERSION
    del backend.responses[("GET", "/v1/delivery-estimates")]
    assert await hass.config_entries.async_reload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.state is ConfigEntryState.LOADED
    assert ir.async_get(hass).async_get_issue(DOMAIN, issue_id) is None
