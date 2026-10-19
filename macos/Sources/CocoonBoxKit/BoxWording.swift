import Foundation

// BoxWording is what the app shows for a box's screen, a desktop or a phone, from the automation its Hello names.
public struct BoxWording: Equatable, Sendable {
    public let noun: String
    public let symbol: String
    public let saveHelp: String
    public let saveMessage: String
    // browser is whether the box has a browser for password sign-in, and a desktop to offload a coding task into.
    public let browser: Bool

    public init(_ automation: Cocoonbox_Agent_V1_Automation?) {
        if automation == .android {
            noun = "phone"
            symbol = "iphone"
            saveHelp = "Save the phone's apps and data now."
            saveMessage = "The phone's apps and data are saved to storage."
            browser = false
        } else {
            noun = "desktop"
            symbol = "display"
            saveHelp = "Save files, logins and agent history now. The box's browser closes during the save."
            saveMessage = "Files, logins and agent history are saved to storage. The browser in your box closes during the save."
            browser = true
        }
    }
}
