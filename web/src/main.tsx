import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '@a1knla/cakeui/style.css'

import { App } from './App'

import './theme.scss'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>
)
