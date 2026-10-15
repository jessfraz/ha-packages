"""Exercise the real config-flow manager and backend validation."""

import pytest
from homeassistant.config_entries import SOURCE_REAUTH, SOURCE_USER
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType

from custom_components.shipments.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN

from .conftest import Backend


async def test_user_flow(hass: HomeAssistant, backend: Backend) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    assert result["type"] is FlowResultType.FORM
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
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
        data={CONF_BACKEND_URL: url, CONF_TOKEN: "token"},
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {CONF_BACKEND_URL: "invalid_url"}


@pytest.mark.parametrize(
    "status,error", [(401, "invalid_auth"), (500, "cannot_connect")]
)
async def test_backend_errors(
    hass: HomeAssistant, backend: Backend, status: int, error: str
) -> None:
    backend.responses[("GET", "/v1/system")] = (status, b"{}", {})
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "token"},
    )
    assert result["errors"] == {"base": error}


async def test_agent_token_is_not_adapter_credential(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.role = "agent"
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "agent-token"},
    )
    assert result["errors"] == {"base": "owner_required"}


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
