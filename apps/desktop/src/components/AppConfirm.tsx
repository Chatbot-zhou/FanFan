import { ExclamationCircleOutlined } from "@ant-design/icons";
import { Input, Modal, message } from "antd";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";

interface ConfirmActionOptions {
  actionKey: string;
  title: string;
  description: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
  confirmPhrase?: string;
}

export function confirmAction({
  actionKey,
  title,
  description,
  confirmLabel = "确认",
  cancelLabel = "取消",
  danger = false,
  confirmPhrase,
}: ConfirmActionOptions): Promise<boolean> {
  let phrase = "";
  recordDiagnosticEvent({
    level: "info",
    component: "frontend.confirmation",
    event_name: "confirmation.opened",
    fields: { action_key: actionKey, requires_phrase: Boolean(confirmPhrase) },
  });
  return new Promise((resolve) => {
    let settled = false;
    const settle = (confirmed: boolean, reason: string) => {
      if (settled) return;
      settled = true;
      recordDiagnosticEvent({
        level: confirmed ? "info" : "warning",
        component: "frontend.confirmation",
        event_name: confirmed ? "confirmation.confirmed" : "feature.action_blocked",
        fields: { action_key: actionKey, reason },
      });
      resolve(confirmed);
    };
    Modal.confirm({
      title,
      icon: <ExclamationCircleOutlined />,
      content: (
        <div className="app-confirm__content">
          <p>{description}</p>
          {confirmPhrase && (
            <label>
              输入 <strong>{confirmPhrase}</strong> 继续
              <Input aria-label="确认短语" autoComplete="off" onChange={(event) => { phrase = event.target.value; }} />
            </label>
          )}
        </div>
      ),
      okText: confirmLabel,
      cancelText: cancelLabel,
      okButtonProps: { danger },
      centered: true,
      className: "app-confirm",
      onOk: () => {
        if (confirmPhrase && phrase.trim() !== confirmPhrase) {
          recordDiagnosticEvent({
            level: "warning",
            component: "frontend.confirmation",
            event_name: "feature.action_blocked",
            fields: { action_key: actionKey, reason: "confirmation_phrase_mismatch" },
          });
          void message.warning("确认短语不匹配，请完整输入后再继续。");
          return Promise.reject(new Error("confirmation phrase mismatch"));
        }
        settle(true, "confirmed");
      },
      onCancel: () => settle(false, "user_cancelled"),
    });
  });
}
