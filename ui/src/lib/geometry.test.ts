import { describe, expect, it } from 'vitest';
import { physicalPointToCss, pointInRect } from './geometry';

describe('desktop drag geometry', () => {
  it('converts physical pixels before comparing with a CSS-pixel drop zone', () => {
    const point = physicalPointToCss({ x: 450, y: 250 }, 2);

    expect(point).toEqual({ x: 225, y: 125 });
    expect(pointInRect(point, { left: 200, right: 300, top: 100, bottom: 200 })).toBe(true);
  });

  it('falls back safely when a WebView reports an invalid scale', () => {
    expect(physicalPointToCss({ x: 12, y: 18 }, 0)).toEqual({ x: 12, y: 18 });
  });
});
