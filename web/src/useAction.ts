import { useRef, useState } from 'react'

export function useAction() {
  const running = useRef(false)
  const [busy, setBusy] = useState(false)
  const [message, setMessage] = useState('')
  const [error, setError] = useState(false)
  async function run(action: () => Promise<unknown>, success = '') {
    if (running.current) return
    running.current = true
    setBusy(true)
    setMessage('')
    setError(false)
    try {
      const result = await action()
      setMessage(result === false ? '' : success)
    } catch (error) {
      setError(true)
      setMessage(error instanceof Error ? error.message : String(error))
    } finally {
      running.current = false
      setBusy(false)
    }
  }
  return { busy, message, error, run }
}
