import ServiceManagement

enum LaunchAtLogin {
    enum Action: Equatable { case none, register, unregister }

    static var status: SMAppService.Status { SMAppService.mainApp.status }
    static var isEnabled: Bool { status == .enabled }
    /// What the toggle shows: registered counts as on even while it awaits approval.
    static func isOn(_ status: SMAppService.Status) -> Bool { status == .enabled || status == .requiresApproval }
    /// Nothing to do when the toggle already matches the service (e.g. the revert after an error),
    /// so re-entering `onChange` never re-registers.
    static func action(toggleOn on: Bool, status: SMAppService.Status) -> Action {
        on == isOn(status) ? .none : (on ? .register : .unregister)
    }
    static let approvalMessage = "Allow RoomMesh in System Settings › General › Login Items to finish."

    /// Applies the toggle. Returns the resulting toggle state and a message to show inline (nil if none).
    @MainActor static func apply(_ on: Bool) -> (isOn: Bool, message: String?) {
        var message: String?
        switch action(toggleOn: on, status: status) {
        case .none: break
        case .register: do { try SMAppService.mainApp.register() } catch { message = describe(error) }
        case .unregister: do { try SMAppService.mainApp.unregister() } catch { message = describe(error) }
        }
        let now = status
        if on, now == .requiresApproval {
            SMAppService.openSystemSettingsLoginItems()
            message = approvalMessage
        }
        return (isOn(now), message)
    }
}
