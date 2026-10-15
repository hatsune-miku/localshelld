import { describe, expect, it } from 'vitest'

import theme from '../src/theme.scss?raw'

describe('application theme', () => {
  it('leaves font families to the component library defaults', () => {
    expect(theme).not.toMatch(/font-family\s*:|--cake-(?:font|mono)\s*:|@font-face/)
  })
})
