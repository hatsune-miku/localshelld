import './index.scss'

export function Feedback({ message, error }: { message: string; error: boolean }) {
  return (
    <p className="feedback" data-error={error} role={error ? 'alert' : 'status'}>
      {message}
    </p>
  )
}
