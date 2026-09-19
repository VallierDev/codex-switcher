import React, { useState } from 'react';
import ReactDOM from 'react-dom/client';
import { AddAccountModal } from '../../src/components/AddAccountModal';
import '../../src/App.css';

function Fixture() {
    const [open, setOpen] = useState(true);
    return <><button id="fixture-open" onClick={() => setOpen(true)}>Open fixture</button>
        <AddAccountModal isOpen={open} onClose={() => setOpen(false)} onAdd={async () => {}} /></>;
}
ReactDOM.createRoot(document.getElementById('root')!).render(<Fixture />);
