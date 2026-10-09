import { getTransport } from "./transport"
import type {
  NativeCapabilities,
  NativeOperation,
  NativeOperationParams,
} from "./native-session"

export function acpNativeCapabilities(
  connectionId: string
): Promise<NativeCapabilities> {
  return getTransport().call("acp_native_capabilities", { connectionId })
}
export function acpNativeOperation<O extends NativeOperation>(
  connectionId: string,
  operation: O,
  params: NativeOperationParams[O]
): Promise<unknown> {
  // A runtime guard also protects JS callers; session identity belongs to the backend.
  if ("sessionId" in params)
    return Promise.reject(new Error("Session identity is bound by the backend"))
  return getTransport().call("acp_native_operation", {
    connectionId,
    operation,
    params,
  })
}
