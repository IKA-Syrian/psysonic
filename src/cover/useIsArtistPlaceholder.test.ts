import { describe, expect, it } from 'vitest';
import { isArtistPlaceholderPixels } from './useIsArtistPlaceholder';

function sample(pixel: (x: number, y: number) => [number, number, number]): Uint8ClampedArray {
  const data = new Uint8ClampedArray(16 * 16 * 4);
  for (let y = 0; y < 16; y++) for (let x = 0; x < 16; x++) {
    const [r, g, b] = pixel(x, y);
    data.set([r, g, b, 255], (y * 16 + x) * 4);
  }
  return data;
}

describe('isArtistPlaceholderPixels', () => {
  it('recognises the grey star placeholder', () => {
    expect(isArtistPlaceholderPixels(sample((x, y) => (Math.abs(x - 7.5) + Math.abs(y - 7.5) < 4 ? [255, 255, 255] : [235, 235, 235])))).toBe(true);
  });
  it('rejects a flat grey image, a white image and a coloured photo', () => {
    expect(isArtistPlaceholderPixels(sample(() => [235, 235, 235]))).toBe(false);
    expect(isArtistPlaceholderPixels(sample(() => [255, 255, 255]))).toBe(false);
    expect(isArtistPlaceholderPixels(sample((x, y) => [x * 16, y * 16, 120]))).toBe(false);
  });
});
