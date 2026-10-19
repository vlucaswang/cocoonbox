import CocoonBoxKit
import Testing

// BoxWordingTests checks the app's words for a phone and a desktop, from the automation a Hello names.
@Suite struct BoxWordingTests {
    @Test func aPhoneHasPhoneWordsAndNoBrowserAndEveryOtherBoxIsADesktop() {
        let phone = BoxWording(.android)
        #expect(phone.noun == "phone" && phone.symbol == "iphone" && !phone.browser)
        #expect(!phone.saveHelp.contains("browser") && !phone.saveMessage.contains("browser"))
        for automation in [Cocoonbox_Agent_V1_Automation.desk, .plain, .unspecified, nil] {
            let desktop = BoxWording(automation)
            #expect(desktop.noun == "desktop" && desktop.symbol == "display" && desktop.browser, "\(String(describing: automation))")
        }
    }
}
