import Foundation

// ImageChoice is one family New Box offers: its label and the CreateBoxRequest.image that picks it.
public struct ImageChoice: Equatable, Sendable {
    public let label: String
    public let image: String

    // choices gives one choice per family of offered, in its order, and none when only one family is offered.
    public static func choices(_ offered: [String]) -> [ImageChoice] {
        var families: [String] = []
        for image in offered {
            let family = family(of: image)
            if !families.contains(family) { families.append(family) }
        }
        guard families.count > 1 else { return [] }
        var labels: Set<String> = []
        return families.map { family in
            let label = labels.insert(label(of: family)).inserted ? label(of: family) : family
            return ImageChoice(label: label, image: family)
        }
    }

    // family is the image's name before the tag, the ':' after the last '/', as the api reads it.
    static func family(of image: String) -> String {
        let name = image.lastIndex(of: "/").map { image.index(after: $0) } ?? image.startIndex
        return String(image[..<(image[name...].firstIndex(of: ":") ?? image.endIndex)])
    }

    static func label(of family: String) -> String {
        switch family.split(separator: "/").last.map(String.init) ?? family {
        case "cbdroid": "Android"
        case "cbcore", "cbdesk": "Desktop"
        default: family
        }
    }
}
