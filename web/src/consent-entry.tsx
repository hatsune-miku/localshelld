import { createRoot } from 'react-dom/client'

import '@a1knla/cakeui/style.css'

import { Consent } from './Consent'

import './theme.scss'

createRoot(document.getElementById('root')!).render(<Consent />)
