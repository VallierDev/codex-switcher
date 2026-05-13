import { useState, useEffect } from 'react';
import { Zap } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { save } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { useAccounts } from './hooks/useAccounts';
import { useUsage } from './hooks/useUsage';
import { AddAccountModal } from './components/AddAccountModal';
import { AddRelayModal } from './components/AddRelayModal';
import { Dashboard } from './components/Dashboard';
import { AccountList } from './components/AccountList';
import { Settings } from './components/Settings';
import { Proxy } from './components/Proxy';
import { Stats } from './components/Stats';
import { Skills } from './components/Skills';
import { SessionRoutes } from './components/SessionRoutes';
import CachePanel from './components/CachePanel';
import { ConfirmModal } from './components/ConfirmModal';
import { RelayImportConfirm } from './components/RelayImportConfirm';
import './App.css';

type PageType = 'dashboard' | 'accounts' | 'proxy' | 'routes' | 'stats' | 'cache' | 'skills' | 'settings';

function App() {
  const {
    accounts,
    currentId,
    settings,
    loading,
    error,
    refresh,
    importCurrent,
    switchTo,
    deleteAccount,
    exportAccounts,
    reloadIdeWindows,
    updateSettings,
    checkSyncConflict,
    getSyncStatus,
    syncActiveWithDisk,
  } = useAccounts();

  const {
    usage,
    loading: usageLoading,
    error: usageError,
    refresh: refreshUsage,
  } = useUsage();

  const [currentPage, setCurrentPage] = useState<PageType>(() => {
    const saved = localStorage.getItem('currentPage');
    return (saved as PageType) || 'dashboard';
  });

  // 持久化当前 tab
  useEffect(() => {
    localStorage.setItem('currentPage', currentPage);
  }, [currentPage]);
  const [showAddModal, setShowAddModal] = useState(false);
  const [showRelayModal, setShowRelayModal] = useState(false);
  const [schedulerError, setSchedulerError] = useState<string | null>(null);

  // 冲突确认弹窗状态
  const [showConflictModal, setShowConflictModal] = useState(false);
  const [conflictAccountName, setConflictAccountName] = useState('');
  const [pendingSwitchId, setPendingSwitchId] = useState<string | null>(null);
  const [isSwitching, setIsSwitching] = useState(false);
  const [syncStatus, setSyncStatus] = useState<any>(null);
  const [proxyRunning, setProxyRunning] = useState(false);

  const checkProxyStatus = async () => {
    try {
      const s = await invoke<{ is_running: boolean }>('get_proxy_status');
      setProxyRunning(s.is_running);
    } catch { setProxyRunning(false); }
  };

  const checkSyncStatus = async () => {
    try {
      const status = await getSyncStatus();
      setSyncStatus(status);
    } catch (err) {
      console.error('检查同步状态失败:', err);
    }
  };

  useEffect(() => {
    checkSyncStatus();
    checkProxyStatus();
  }, []);

  const currentAccount = accounts.find(a => a.id === currentId) || null;

  const classifyRefreshFailure = (reason: string): 'permanent' | 'transient' => {
    const lower = reason.toLowerCase();
    if (
      lower.includes('refresh_token_reused') ||
      lower.includes('refresh_token_invalidated') ||
      lower.includes('refresh_token_expired')
    ) {
      return 'permanent';
    }
    return 'transient';
  };

  // 监听后台调度器的账号更新事件
  useEffect(() => {
    const unlisten = listen('accounts-updated', () => {
      console.log('[Frontend] 收到后台刷新通知，重新加载账号列表');
      refresh();
    });

    return () => {
      unlisten.then(f => f());
    };
  }, [refresh]);

  // 监听后台刷新失败事件
  useEffect(() => {
    const unlisten = listen<{ account_name: string; reason: string }>('token-refresh-failed', (event) => {
      const { account_name, reason } = event.payload;
      const timestamp = new Date().toLocaleTimeString();
      const kind = classifyRefreshFailure(reason);
      if (kind === 'permanent') {
        setSchedulerError(`后台保活已停用（${account_name}，需重新登录）@ ${timestamp}`);
      } else {
        setSchedulerError(`后台保活临时失败（${account_name}）：${reason} @ ${timestamp}`);
      }
    });

    return () => {
      unlisten.then(f => f());
    };
  }, []);

  // 监听代理切号/封号事件
  const [proxyNotice, setProxyNotice] = useState<string | null>(null);
  useEffect(() => {
    const unsub1 = listen<string>('proxy-account-switched', (e) => {
      const msg = `代理已自动切号 → ${e.payload}`;
      setProxyNotice(msg);
      setTimeout(() => setProxyNotice(null), 8000);
      refresh();
      checkProxyStatus();
    });
    const unsub2 = listen<string>('proxy-account-banned', (e) => {
      const msg = `检测到封号: ${e.payload}，已自动切换`;
      setProxyNotice(msg);
      setTimeout(() => setProxyNotice(null), 10000);
      refresh();
    });
    const unsub3 = listen<string>('proxy-all-exhausted', (e) => {
      setProxyNotice(e.payload);
      setTimeout(() => setProxyNotice(null), 15000);
    });
    return () => {
      unsub1.then(f => f());
      unsub2.then(f => f());
      unsub3.then(f => f());
    };
  }, [refresh]);

  // 监听设置更新事件
  useEffect(() => {
    const unlisten = listen('settings-updated', () => {
      console.log('[Frontend] 收到设置更新通知，重新加载设置');
      refresh();
      checkProxyStatus();
    });

    return () => {
      unlisten.then(f => f());
    };
  }, [refresh]);

  // 执行真正的切换逻辑
  const performSwitch = async (id: string) => {
    await switchTo(id);
    if (settings.auto_reload_ide) {
      setTimeout(async () => {
        await reloadIdeWindows(false);
      }, 300);
    }
    setTimeout(() => {
      refreshUsage();
    }, 500);
  };

  // 切换账号（带冲突检测）
  const handleSwitch = async (id: string) => {
    if (isSwitching) return;
    try {
      setIsSwitching(true);
      // 1. 检查是否有未同步的官方 Token 更新
      const conflictName = await checkSyncConflict();

      if (conflictName) {
        // 2. 如果有冲突，暂存目标 ID，弹出确认框
        setConflictAccountName(conflictName);
        setPendingSwitchId(id);
        setShowConflictModal(true);
        return;
      }

      // 3. 无冲突直接切换
      await performSwitch(id);
    } catch (err) {
      console.error('切换检查失败:', err);
      // 尝试保守切换
      try {
        await performSwitch(id);
      } catch (switchErr) {
        // switchTo 内部已经 setError 了，但我们这里可以再打印一下
        console.error('保守切换也失败了:', switchErr);
      }
    } finally {
      setIsSwitching(false);
      checkSyncStatus();
    }
  };

  // 确认覆盖
  const handleConfirmSwitch = async () => {
    if (!pendingSwitchId || isSwitching) return;
    try {
      setIsSwitching(true);
      await performSwitch(pendingSwitchId);
      setShowConflictModal(false);
      setPendingSwitchId(null);
    } catch (err) {
      console.error('确认切换失败:', err);
      // switchTo 内部已经 setError，这里关闭弹窗即可，让用户看到 Banner 错误
      setShowConflictModal(false);
    } finally {
      setIsSwitching(false);
      checkSyncStatus();
    }
  };

  // 以 IDE 状态为准
  const handleFollowIdeAction = async () => {
    try {
      setIsSwitching(true);
      await syncActiveWithDisk();
      setShowConflictModal(false);
      setPendingSwitchId(null);
      await checkSyncStatus();
    } catch (err) {
      console.error('同步 IDE 状态失败:', err);
    } finally {
      setIsSwitching(false);
    }
  };

  // 取消切换
  const handleCancelSwitch = () => {
    setShowConflictModal(false);
    setPendingSwitchId(null);
  };

  const handleExport = async () => {
    try {
      const json = await exportAccounts();
      const path = await save({
        filters: [{
          name: 'JSON',
          extensions: ['json']
        }],
        defaultPath: `codex-accounts-${new Date().toISOString().slice(0, 10)}.json`
      });

      if (path) {
        await writeTextFile(path, json);
        alert('导出成功！');
      }
    } catch (err) {
      alert('导出失败: ' + String(err));
    }
  };


  if (loading) {
    return (
      <div className="app" data-palette={settings.theme_palette || 'github'}>
        <div className="loading">
          <div className="spinner" />
          <p>加载中...</p>
        </div>
      </div>
    );
  }

  return (
    <div className="app" data-palette={settings.theme_palette || 'github'}>
      {/* 顶部标题栏 */}
      <header className="app-header">
        <div className="header-left">
          <div className="app-logo">
            <Zap size={18} />
          </div>
          <h1>Codex Switcher <span className="app-version">v0.6.0</span></h1>
          <div className={`proxy-indicator ${proxyRunning ? 'on' : 'off'}`} title={proxyRunning ? '代理运行中' : '代理未启动'}>
            <span className="proxy-dot" />
            {proxyRunning ? 'Proxy ON' : 'Proxy OFF'}
          </div>
        </div>

        {/* 导航菜单 */}
        <nav className="header-nav">
          <button
            className={`nav-item ${currentPage === 'dashboard' ? 'active' : ''}`}
            onClick={() => setCurrentPage('dashboard')}
          >
            仪表盘
          </button>
          <button
            className={`nav-item ${currentPage === 'accounts' ? 'active' : ''}`}
            onClick={() => setCurrentPage('accounts')}
          >
            账号管理
          </button>
          <button
            className={`nav-item ${currentPage === 'proxy' ? 'active' : ''}`}
            onClick={() => setCurrentPage('proxy')}
          >
            代理
          </button>
          <button
            className={`nav-item ${currentPage === 'routes' ? 'active' : ''}`}
            onClick={() => setCurrentPage('routes')}
          >
            路由
          </button>
          <button
            className={`nav-item ${currentPage === 'stats' ? 'active' : ''}`}
            onClick={() => setCurrentPage('stats')}
          >
            统计
          </button>
          <button
            className={`nav-item ${currentPage === 'cache' ? 'active' : ''}`}
            onClick={() => setCurrentPage('cache')}
          >
            缓存
          </button>
          <button
            className={`nav-item ${currentPage === 'skills' ? 'active' : ''}`}
            onClick={() => setCurrentPage('skills')}
          >
            Skills
          </button>
          <button
            className={`nav-item ${currentPage === 'settings' ? 'active' : ''}`}
            onClick={() => setCurrentPage('settings')}
          >
            设置
          </button>
        </nav>

        <div className="header-actions">
          <button className="btn btn-primary" onClick={() => setShowAddModal(true)}>
            + 登录账号
          </button>
          <button className="btn btn-relay" onClick={() => setShowRelayModal(true)}>
            + 添加中转
          </button>
        </div>
      </header>

      {(error || schedulerError) && (
        <div className="error-banner">
          {error && <div>{error}</div>}
          {schedulerError && <div>{schedulerError}</div>}
        </div>
      )}

      {proxyNotice && (
        <div className="proxy-notice-banner" onClick={() => setProxyNotice(null)}>
          {proxyNotice}
        </div>
      )}

      <main className="app-main">
        {currentPage === 'dashboard' ? (
          <Dashboard
            accounts={accounts}
            currentAccount={currentAccount}
            usage={usage}
            usageLoading={usageLoading}
            usageError={usageError}
            isCurrentInvalid={currentAccount?.cached_quota?.is_valid_for_cli === false}
            onSwitch={handleSwitch}
            onRefreshUsage={refreshUsage}
            onNavigateToAccounts={() => setCurrentPage('accounts')}
            onExport={handleExport}
            proxyRunning={proxyRunning}
            syncStatus={syncStatus}
            onSyncWithDisk={async () => {
              try {
                await syncActiveWithDisk();
                checkSyncStatus();
              } catch (err) {
                console.error('同步状态失败:', err);
              }
            }}
            onImportDiskAccount={async (name) => {
              try {
                await importCurrent(name, '从 IDE 自动导入');
                checkSyncStatus();
              } catch (err) {
                console.error('导入失败:', err);
              }
            }}
            onForceOverwriteDisk={async () => {
              try {
                await invoke<string>('force_overwrite_disk_with_current');
                if (settings.auto_reload_ide) {
                  await reloadIdeWindows(false);
                }
                checkSyncStatus();
                refreshUsage();
              } catch (err) {
                console.error('覆盖 ~/.codex/auth.json 失败:', err);
              }
            }}
          />
        ) : currentPage === 'accounts' ? (
          <AccountList
            accounts={accounts}
            currentId={currentId}
            settings={settings}
            onSwitch={handleSwitch}
            onDelete={deleteAccount}
            onUpdateSettings={updateSettings}
            onRefreshComplete={refresh}
            onAddAccount={() => setShowAddModal(true)}
            onAddRelay={() => setShowRelayModal(true)}
            onRefreshUsage={refreshUsage}
            usageLoading={usageLoading}
          />
        ) : currentPage === 'proxy' ? (
          <Proxy />
        ) : currentPage === 'routes' ? (
          <SessionRoutes />
        ) : currentPage === 'stats' ? (
          <Stats />
        ) : currentPage === 'cache' ? (
          <CachePanel accounts={accounts.map(a => ({ id: a.id, name: a.name }))} />
        ) : currentPage === 'skills' ? (
          <Skills />
        ) : (
          <Settings />
        )}
      </main>

      <AddAccountModal
        isOpen={showAddModal}
        onClose={() => setShowAddModal(false)}
        onAdd={importCurrent}
        onSuccess={refresh}
      />

      <AddRelayModal
        isOpen={showRelayModal}
        onClose={() => setShowRelayModal(false)}
        onSuccess={refresh}
      />

      <ConfirmModal
        isOpen={showConflictModal}
        title="⚠️ 登录状态冲突警告"
        message={
          <>
            <p>检测到官方 Codex 插件中存在未同步的 Token 更新。</p>
            <p>当前的账号状态与官方文件不一致：</p>
            <span className="confirm-account-name">{conflictAccountName || '当前账号'}</span>
            <p style={{ marginTop: '12px' }}>
              直接切换将<b>覆盖</b>官方插件中的当前登录状态，且无法找回这些未同步的更新。
            </p>
          </>
        }
        confirmText="确认覆盖并切换"
        cancelText="取消"
        onConfirm={handleConfirmSwitch}
        onCancel={handleCancelSwitch}
        isLoading={isSwitching}
        extraActionText="以 IDE 为准 (同步状态)"
        onExtraAction={handleFollowIdeAction}
      />

      <RelayImportConfirm />
    </div>
  );
}

export default App;
