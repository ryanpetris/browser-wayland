import { createRoot } from 'react-dom/client';
import './index.css';
import { createViewer } from './viewer.js';
import { App } from './App.jsx';

// The engine lives here, in the entry module: an edit under hot reload re-runs App.jsx, not this.
createRoot(document.getElementById('root')).render(<App viewer={createViewer()} />);
