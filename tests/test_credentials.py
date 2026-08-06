from __future__ import annotations

import pytest

from openrouter_video_studio.credentials import CredentialStore


class GoodBackend:
    priority = 5


class FakeKeyring:
    def __init__(self) -> None:
        self.value: str | None = None
        self.calls: list[tuple[str, str, str | None]] = []

    def get_keyring(self) -> GoodBackend:
        return GoodBackend()

    def get_password(self, service: str, username: str) -> str | None:
        self.calls.append(("get", service, username))
        return self.value

    def set_password(self, service: str, username: str, value: str) -> None:
        self.calls.append(("set", service, username))
        self.value = value

    def delete_password(self, service: str, username: str) -> None:
        self.calls.append(("delete", service, username))
        self.value = None


def test_credential_store_round_trips_through_available_keyring() -> None:
    keyring = FakeKeyring()
    store = CredentialStore(keyring_module=keyring)

    assert store.status().persistent is True
    assert store.set("  sk-test-secret  ") is True
    assert store.get() == "sk-test-secret"
    assert store.delete() is True
    assert store.get() is None
    assert [call[0] for call in keyring.calls] == ["set", "get", "delete", "get"]


def test_missing_keyring_uses_process_memory_only() -> None:
    store = CredentialStore(keyring_module=None)

    assert store.status().persistent is False
    assert store.set("sk-memory-only") is False
    assert store.get() == "sk-memory-only"
    assert store.delete() is False
    assert store.get() is None


def test_keyring_write_failure_degrades_without_losing_in_memory_key() -> None:
    class BrokenKeyring(FakeKeyring):
        def set_password(self, service: str, username: str, value: str) -> None:
            raise RuntimeError(f"backend echoed {value}")

    store = CredentialStore(keyring_module=BrokenKeyring())

    assert store.set("sk-never-display") is False
    assert store.get() == "sk-never-display"
    status = store.status()
    assert status.backend == "memory"
    assert status.persistent is False
    assert "sk-never-display" not in status.message


def test_zero_priority_backend_is_not_treated_as_secure_storage() -> None:
    class ZeroPriorityBackend:
        priority = 0

    class UnsupportedKeyring(FakeKeyring):
        def get_keyring(self) -> ZeroPriorityBackend:
            return ZeroPriorityBackend()

    store = CredentialStore(keyring_module=UnsupportedKeyring())

    assert store.persistent_available is False
    assert store.status().backend == "memory"


@pytest.mark.parametrize("invalid", ["", "   ", "sk-test secret", "sk-test\nsecret"])
def test_credential_store_rejects_empty_or_whitespace_keys(invalid: str) -> None:
    store = CredentialStore(keyring_module=None)

    with pytest.raises(ValueError):
        store.set(invalid)

    assert store.get() is None
