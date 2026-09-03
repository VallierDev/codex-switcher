// Dev-only component fixture. No account store or credentials are loaded.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { AddRelayModal } from '../../src/components/AddRelayModal';
import '../../src/App.css';

function Preview() {
    const [open, setOpen] = useState(true);
    return <main data-theme="dark">
        <button onClick={() => setOpen(true)}>打开中转预设</button>
        <AddRelayModal isOpen={open} onClose={() => setOpen(false)} />
    </main>;
}
createRoot(document.getElementById('root')!).render(<Preview />);
