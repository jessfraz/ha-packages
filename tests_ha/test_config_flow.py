"""Exercise the real config-flow manager and backend validation."""

from dataclasses import dataclass, field
from uuid import UUID

import pytest
from aiohasupervisor import SupervisorClient
from aiohttp import web
from homeassistant.config_entries import (
    SOURCE_HASSIO,
    SOURCE_REAUTH,
    SOURCE_USER,
    ConfigFlowResult,
)
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.hassio import HassioServiceInfo
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import Backend


async def standalone_flow(
    hass: HomeAssistant, data: dict[str, str]
) -> ConfigFlowResult:
    """Use the standalone choice exposed by Home Assistant's real flow manager."""
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    return await hass.config_entries.flow.async_configure(result["flow_id"], data)


async def test_user_flow(hass: HomeAssistant, backend: Backend) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    assert result["type"] is FlowResultType.MENU
    assert result["menu_options"] == ["app", "standalone"]
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "standalone"
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: f"{backend.url}/", CONF_TOKEN: "owner-token"},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["data"] == {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "owner-token"}
    assert backend.requests[0].headers["Authorization"] == "Bearer owner-token"
    await hass.async_block_till_done()


@pytest.mark.parametrize(
    "url",
    [
        "ftp://localhost",
        "http://user:password@localhost",
        "http://localhost/v1",
        "http://localhost?url=https://elsewhere.invalid",
        "http://localhost#fragment",
        "http://localhost:0",
        "http://localhost:invalid",
        "http://[malformed",
        "http://local host",
        "http://localhost\\@elsewhere.invalid",
        "http:///missing-host",
    ],
)
async def test_reject_invalid_origin(hass: HomeAssistant, url: str) -> None:
    result = await standalone_flow(hass, {CONF_BACKEND_URL: url, CONF_TOKEN: "token"})
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {CONF_BACKEND_URL: "invalid_url"}


@pytest.mark.parametrize(
    "status,error", [(401, "invalid_auth"), (500, "cannot_connect")]
)
async def test_backend_errors(
    hass: HomeAssistant, backend: Backend, status: int, error: str
) -> None:
    backend.responses[("GET", "/v1/system")] = (status, b"{}", {})
    result = await standalone_flow(
        hass, {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "token"}
    )
    assert result["errors"] == {"base": error}


async def test_agent_token_is_not_adapter_credential(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.role = "agent"
    result = await standalone_flow(
        hass, {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "agent-token"}
    )
    assert result["errors"] == {"base": "integration_required"}


async def test_single_backend(hass: HomeAssistant, entry) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "single_instance_allowed"


async def test_reauth_updates_existing_entry(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_REAUTH, "entry_id": entry.entry_id},
        data=entry.data,
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "reauth_confirm"
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "replacement-owner-token"},
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "reauth_successful"
    await hass.async_block_till_done()
    assert entry.data[CONF_TOKEN] == "replacement-owner-token"
    assert len(hass.config_entries.async_entries(DOMAIN)) == 1


DISCOVERY_UUID = "019c6e27e55b73d187d84e01f1f75043"
APP_SLUG = "testrepo_shipments"


def app_discovery(
    backend: Backend, *, token: str = "private-integration-token"
) -> HassioServiceInfo:
    return HassioServiceInfo(
        config={CONF_BACKEND_URL: backend.url, CONF_TOKEN: token},
        name="Shipments",
        slug=APP_SLUG,
        uuid=DISCOVERY_UUID,
    )


@dataclass
class SupervisorDiscovery:
    messages: list[dict[str, object]] = field(default_factory=list)
    status: int = 200
    requests: int = 0

    async def handle(self, request: web.Request) -> web.Response:
        self.requests += 1
        assert request.headers["Authorization"] == "Bearer test-supervisor-token"
        return web.json_response(
            {"result": "ok", "data": {"discovery": self.messages, "services": {}}},
            status=self.status,
        )


@pytest.fixture
async def supervisor(hass: HomeAssistant, aiohttp_server, socket_enabled, monkeypatch):
    state = SupervisorDiscovery()
    app = web.Application()
    app.router.add_get("/discovery", state.handle)
    server = await aiohttp_server(app)
    client = SupervisorClient(
        str(server.make_url("")).rstrip("/"),
        "test-supervisor-token",
        session=async_get_clientsession(hass),
    )
    monkeypatch.setattr(
        "custom_components.shipments.config_flow.is_hassio", lambda hass: True
    )
    monkeypatch.setattr(
        "homeassistant.components.hassio.get_supervisor_client",
        lambda hass: client,
    )
    return state


async def test_discovery_requires_confirmation_before_contact_or_save(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.role = "integration"
    info = app_discovery(backend)
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert result["data_schema"] is not None
    assert result["data_schema"].schema == {}
    assert not backend.requests
    assert not hass.config_entries.async_entries(DOMAIN)
    assert info.config[CONF_TOKEN] not in str(result)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["result"].unique_id == DISCOVERY_UUID
    assert result["data"] == {
        CONF_BACKEND_URL: backend.url,
        CONF_TOKEN: "private-integration-token",
        CONF_ADDON_SLUG: APP_SLUG,
    }
    assert (
        backend.requests[0].headers["Authorization"]
        == "Bearer private-integration-token"
    )
    await hass.async_block_till_done()


async def test_user_app_choice_uses_authenticated_supervisor_discovery(
    hass: HomeAssistant, backend: Backend, supervisor: SupervisorDiscovery
) -> None:
    backend.role = "integration"
    supervisor.messages.append(
        {
            "addon": APP_SLUG,
            "service": DOMAIN,
            "uuid": DISCOVERY_UUID,
            "config": {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "private-token"},
        }
    )
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "app"}
    )
    assert supervisor.requests == 1
    assert result["step_id"] == "hassio_confirm"
    assert not backend.requests
    assert "private-token" not in str(result)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["data"][CONF_ADDON_SLUG] == APP_SLUG
    await hass.async_block_till_done()


async def test_missing_app_can_be_started_then_retried(
    hass: HomeAssistant, backend: Backend, supervisor: SupervisorDiscovery
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "app"}
    )
    assert result["step_id"] == "app"
    assert result["errors"] == {"base": "app_not_found"}
    supervisor.messages.append(
        {
            "addon": APP_SLUG,
            "service": DOMAIN,
            "uuid": DISCOVERY_UUID,
            "config": {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "private-token"},
        }
    )
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["step_id"] == "hassio_confirm"
    assert not backend.requests


async def test_app_choice_on_standalone_home_assistant(hass: HomeAssistant) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "app"}
    )
    assert result["step_id"] == "app"
    assert result["errors"] == {"base": "supervisor_required"}


async def test_supervisor_unavailable_has_recoverable_guidance(
    hass: HomeAssistant, supervisor: SupervisorDiscovery
) -> None:
    supervisor.status = 503
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "app"}
    )
    assert result["step_id"] == "app"
    assert result["errors"] == {"base": "supervisor_unavailable"}


async def test_discovered_backend_auth_error_does_not_expose_token(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/system")] = (401, b"{}", {})
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=app_discovery(backend)
    )
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["step_id"] == "hassio_confirm"
    assert result["errors"] == {"base": "invalid_auth"}
    assert "private-integration-token" not in str(result)
    assert not hass.config_entries.async_entries(DOMAIN)


@pytest.mark.parametrize(
    "field,value",
    [
        (CONF_BACKEND_URL, "http://localhost/path"),
        (CONF_TOKEN, None),
        (CONF_TOKEN, "token\nsecond-header"),
    ],
)
async def test_invalid_discovery_does_not_contact_backend(
    hass: HomeAssistant, backend: Backend, field: str, value: object
) -> None:
    info = app_discovery(backend)
    info.config[field] = value
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "invalid_discovery"
    assert not backend.requests


async def test_repeated_discovery_keeps_one_confirmation(
    hass: HomeAssistant, backend: Backend
) -> None:
    info = app_discovery(backend)
    first = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    second = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    assert second["type"] is FlowResultType.ABORT
    assert second["reason"] == "already_in_progress"
    assert first["flow_id"] in {
        item["flow_id"] for item in hass.config_entries.flow.async_progress()
    }
    assert not backend.requests


async def test_restart_rediscovery_keeps_existing_entry(
    hass: HomeAssistant, backend: Backend
) -> None:
    existing = MockConfigEntry(
        domain=DOMAIN,
        unique_id=DISCOVERY_UUID,
        source=SOURCE_HASSIO,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "saved-token",
            CONF_ADDON_SLUG: APP_SLUG,
        },
    )
    existing.add_to_hass(hass)
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=app_discovery(backend)
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "single_instance_allowed"
    assert existing.data[CONF_TOKEN] == "saved-token"
    assert not backend.requests


async def test_app_reauth_discovers_replacement_without_token_form(
    hass: HomeAssistant, backend: Backend, supervisor: SupervisorDiscovery
) -> None:
    backend.role = "integration"
    existing = MockConfigEntry(
        domain=DOMAIN,
        unique_id=DISCOVERY_UUID,
        source=SOURCE_HASSIO,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "old-token",
            CONF_ADDON_SLUG: APP_SLUG,
        },
    )
    existing.add_to_hass(hass)
    supervisor.messages.append(
        {
            "addon": APP_SLUG,
            "service": DOMAIN,
            "uuid": UUID(DISCOVERY_UUID).hex,
            "config": {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "replacement-token"},
        }
    )
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_REAUTH, "entry_id": existing.entry_id},
        data=existing.data,
    )
    assert result["step_id"] == "hassio_confirm"
    assert result["data_schema"] is not None
    assert result["data_schema"].schema == {}
    assert not backend.requests
    assert existing.data[CONF_TOKEN] == "old-token"
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "reauth_successful"
    assert existing.data[CONF_TOKEN] == "replacement-token"
    await hass.async_block_till_done()


async def test_discovered_app_rejects_owner_credentials(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.role = "owner"
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=app_discovery(backend)
    )
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["step_id"] == "hassio_confirm"
    assert result["errors"] == {"base": "integration_required"}
    assert not hass.config_entries.async_entries(DOMAIN)


async def test_user_input_cannot_bypass_setup_choice(
    hass: HomeAssistant, backend: Backend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "private-token"},
    )
    assert result["type"] is FlowResultType.MENU
    assert not backend.requests
