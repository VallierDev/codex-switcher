import './ConfirmModal.css';

interface ConfirmModalProps {
    isOpen: boolean;
    title: string;
    message: React.ReactNode;
    confirmText?: string;
    cancelText?: string;
    onConfirm: () => void;
    onCancel: () => void;
    isLoading?: boolean;
    loadingText?: string;
    extraActionText?: string;
    onExtraAction?: () => void;
}

export function ConfirmModal({
    isOpen,
    title,
    message,
    confirmText = '确认',
    cancelText = '取消',
    onConfirm,
    onCancel,
    isLoading = false,
    loadingText = '正在切换...',
    extraActionText,
    onExtraAction,
}: ConfirmModalProps) {
    if (!isOpen) return null;

    return (
        <div className="modal-overlay" onClick={onCancel}>
            <div className="modal-content confirm-modal" onClick={e => e.stopPropagation()}>
                <div className="confirm-header">
                    <div className="confirm-icon">⚠️</div>
                    <h3 className="confirm-title">{title}</h3>
                </div>

                <div className="confirm-body">
                    {message}
                </div>

                <div className="confirm-footer">
                    <button className="btn-cancel" onClick={onCancel} disabled={isLoading}>
                        {cancelText}
                    </button>
                    {extraActionText && onExtraAction && (
                        <button className="btn-extra" onClick={onExtraAction} disabled={isLoading}>
                            {extraActionText}
                        </button>
                    )}
                    <button className="btn-confirm" onClick={onConfirm} disabled={isLoading}>
                        {isLoading ? loadingText : confirmText}
                    </button>
                </div>
            </div>
        </div>
    );
}
