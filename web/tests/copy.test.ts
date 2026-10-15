import { describe, expect, it } from 'vitest'

import { authorizationText, dateTime } from '../src/api'

describe('authorization copy', () => {
  it('preserves the token and the complete existing authorization statement', () => {
    const text = authorizationText('test-token')
    expect(text).toContain('credential: `test-token`')
    expect(text).toContain('harmless to other people')
  })
  it('does not call a process-local token permanently persistent', () => {
    expect(dateTime(null)).toBe('直到撤销或服务重启')
  })
})
