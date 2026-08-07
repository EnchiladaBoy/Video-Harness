import '@testing-library/jest-dom/vitest';

if (typeof HTMLDialogElement !== 'undefined') {
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
    this.open = true;
  };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
    this.open = false;
  };
}

Object.defineProperty(window, 'matchMedia', {
  writable: true,
  value: (query: string): MediaQueryList =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => true
    }) as MediaQueryList
});

Object.defineProperty(globalThis, 'IntersectionObserver', {
  writable: true,
  value: class IntersectionObserverMock {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
    takeRecords(): IntersectionObserverEntry[] {
      return [];
    }
  }
});

Object.defineProperty(globalThis, 'ResizeObserver', {
  writable: true,
  value: class ResizeObserverMock {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  }
});

Object.defineProperties(HTMLMediaElement.prototype, {
  pause: { configurable: true, value: () => undefined },
  load: { configurable: true, value: () => undefined },
  play: { configurable: true, value: () => Promise.resolve() }
});

Object.defineProperty(Element.prototype, 'scrollIntoView', {
  configurable: true,
  value: () => undefined
});
