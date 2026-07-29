import { describe, expect, it } from 'vitest';

import { developmentRendererUrl } from './renderer-location';

describe('renderer location', () => {
  it('ignores an inherited development URL in packaged builds', () => {
    expect(
      developmentRendererUrl(true, {
        ELECTRON_RENDERER_URL: 'http://127.0.0.1:5175/'
      })
    ).toBeNull();
  });

  it('uses the Electron Vite URL during development', () => {
    expect(
      developmentRendererUrl(false, {
        ELECTRON_RENDERER_URL: 'http://127.0.0.1:5173/'
      })
    ).toBe('http://127.0.0.1:5173/');
  });
});
