import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '@a1knla/cakeui/style.css'

import { Sync } from './Sync'

import './theme.scss'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <Sync />
  </StrictMode>
)
