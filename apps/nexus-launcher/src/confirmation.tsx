import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Modal, ActionButton } from "./ui-components";
import { useI18n } from "./i18n";

type Request = { message: string; resolve: (accepted: boolean) => void };
let receiver: ((request: Request) => void) | undefined;

// Fail closed if the UI has gone away. Never fall back to a native message box.
export function confirmAction(message: string): Promise<boolean> {
  return new Promise((resolve) => (receiver ? receiver({ message, resolve }) : resolve(false)));
}

export function ConfirmationHost() {
  const { t } = useI18n();
  const [request, setRequest] = useState<Request | null>(null);
  useEffect(() => {
    let current: Request | null = null;
    const receive = (next: Request) => {
      if (current) {
        next.resolve(false);
        return;
      }
      current = next;
      setRequest({
        ...next,
        resolve: (accepted) => {
          current = null;
          setRequest(null);
          next.resolve(accepted);
        },
      });
    };
    receiver = receive;
    return () => {
      if (receiver === receive) receiver = undefined;
      current?.resolve(false);
    };
  }, []);
  return (
    request &&
    createPortal(
      <Modal title={t("Confirm action")} onClose={() => request.resolve(false)}>
        <p>{request.message}</p>
        <div className="button-row">
          <ActionButton onClick={() => request.resolve(false)}>{t("Cancel")}</ActionButton>
          <ActionButton onClick={() => request.resolve(true)}>{t("Confirm")}</ActionButton>
        </div>
      </Modal>,
      document.body,
    )
  );
}

export function BusyOverlay({
  label,
  children,
}: {
  label: string | null;
  children?: React.ReactNode;
}) {
  const { t } = useI18n();
  return (
    label &&
    createPortal(
      <Modal title={t("Operation in progress")} locked onClose={() => {}}>
        <p role="status" aria-live="polite">
          {label}
        </p>
        <p>{t("Please wait until this operation finishes before making other changes.")}</p>
        {children}
      </Modal>,
      document.body,
    )
  );
}
