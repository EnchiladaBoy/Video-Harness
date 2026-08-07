export interface Point {
  x: number;
  y: number;
}

export interface Rect {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

export function physicalPointToCss(point: Point, scaleFactor: number): Point {
  const safeScale = Number.isFinite(scaleFactor) && scaleFactor > 0 ? scaleFactor : 1;
  return { x: point.x / safeScale, y: point.y / safeScale };
}

export function pointInRect(point: Point, rect: Rect): boolean {
  return (
    point.x >= rect.left &&
    point.x <= rect.right &&
    point.y >= rect.top &&
    point.y <= rect.bottom
  );
}
