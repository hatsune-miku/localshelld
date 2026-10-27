import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '@a1knla/cakeui/style.css'

import { Maintenance } from './Maintenance'

import './theme.scss'

const token = location.hash.slice(1)
history.replaceState(null, '', location.pathname)

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <Maintenance token={token} />
  </StrictMode>
)
