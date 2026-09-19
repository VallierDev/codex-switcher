interface OAuthLinkProps {
    url: string;
    error: string | null;
    copying: boolean;
    onCopy: () => void;
}

export function OAuthLink({ url, error, copying, onCopy }: OAuthLinkProps) {
    return (
        <div className="oauth-link-panel">
            <label>
                授权链接
                <textarea className="oauth-link-value" value={url} readOnly rows={4}
                    spellCheck={false} onFocus={event => event.currentTarget.select()} />
            </label>
            {error && <p className="error-message" role="alert">{error}</p>}
            <button className="btn btn-ghost btn-full" type="button" disabled={copying} onClick={onCopy}>
                {copying ? '复制中…' : '复制此链接'}
            </button>
        </div>
    );
}
