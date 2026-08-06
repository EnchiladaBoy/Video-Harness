"""API-key storage backed by the OS keyring with an in-memory fallback."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from .config import APP_NAME


DEFAULT_USERNAME = "openrouter-api-key"
_AUTO = object()


@dataclass(frozen=True, slots=True)
class CredentialStatus:
    backend: str
    available: bool
    persistent: bool
    message: str


class CredentialStore:
    """Store one OpenRouter key without ever writing it to plaintext files.

    If Python keyring or its Secret Service backend is unavailable, the key is
    retained only for this object's process lifetime.  Backend failures are
    swallowed deliberately so their exception strings cannot accidentally leak
    credential material into the interface.
    """

    def __init__(
        self,
        *,
        service_name: str = APP_NAME,
        username: str = DEFAULT_USERNAME,
        keyring_module: Any = _AUTO,
    ) -> None:
        self.service_name = service_name
        self.username = username
        self._memory_key: str | None = None
        self._keyring: Any | None = None
        self._status = CredentialStatus(
            backend="memory",
            available=False,
            persistent=False,
            message="System keyring unavailable; key will be kept in memory for this session",
        )

        if keyring_module is _AUTO:
            try:
                import keyring as imported_keyring
            except (ImportError, RuntimeError):
                imported_keyring = None
            keyring_module = imported_keyring
        self._configure_backend(keyring_module)

    def _configure_backend(self, keyring_module: Any | None) -> None:
        if keyring_module is None:
            return
        try:
            backend = keyring_module.get_keyring()
            priority = float(getattr(backend, "priority", 0))
            if priority <= 0:
                return
            backend_name = f"{type(backend).__module__}.{type(backend).__name__}"
        except Exception:  # keyring backends expose several environment-specific errors
            return
        self._keyring = keyring_module
        self._status = CredentialStatus(
            backend=backend_name,
            available=True,
            persistent=True,
            message="API key will be stored in the system keyring",
        )

    @property
    def persistent_available(self) -> bool:
        return self._status.persistent

    def status(self) -> CredentialStatus:
        return self._status

    def get(self) -> str | None:
        if self._keyring is not None:
            try:
                value = self._keyring.get_password(self.service_name, self.username)
            except Exception:
                self._degrade_to_memory()
            else:
                if value:
                    self._memory_key = value
                    return value
        return self._memory_key

    def set(self, api_key: str) -> bool:
        """Store ``api_key`` and return whether it was persisted."""

        key = api_key.strip()
        if not key:
            raise ValueError("API key cannot be empty")
        if any(character.isspace() for character in key):
            raise ValueError("API key cannot contain whitespace")
        self._memory_key = key
        if self._keyring is None:
            return False
        try:
            self._keyring.set_password(self.service_name, self.username, key)
        except Exception:
            self._degrade_to_memory()
            return False
        return True

    def delete(self) -> bool:
        """Forget the key and return whether a persisted key was deleted."""

        self._memory_key = None
        if self._keyring is None:
            return False
        try:
            self._keyring.delete_password(self.service_name, self.username)
        except Exception as exc:
            # A missing password is already the desired outcome.  Avoid importing
            # backend-specific exception classes when keyring itself is optional.
            if exc.__class__.__name__ not in {"PasswordDeleteError", "KeyringError"}:
                self._degrade_to_memory()
            return False
        return True

    def _degrade_to_memory(self) -> None:
        self._keyring = None
        self._status = CredentialStatus(
            backend="memory",
            available=False,
            persistent=False,
            message="System keyring failed; key is kept in memory for this session only",
        )

    # Descriptive aliases are convenient for callers without changing the small
    # get/set/delete contract used by the TUI.
    load_api_key = get
    save_api_key = set
    delete_api_key = delete


__all__ = ["CredentialStatus", "CredentialStore"]

