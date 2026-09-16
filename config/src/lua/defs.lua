--- @meta ChariotAPI

--- @class SourceRef

--- @class PackageRef

--- @class ArchiveSource
--- @field type "archive"
--- @field url string
--- @field checksum string
--- @field king "tar"
--- @field compression "gz"

--- @class GitSource
--- @field type "git"
--- @field url string
--- @field revision string

--- @alias Dependency string|PackageRef|SourceRef

chariot = {}

--- Define a source and return a reference to it.
--- @param base ArchiveSource|GitSource
--- @param patches? string[]
--- @param prepare? { script: string, dependencies: Dependency[] }
--- @return SourceRef
function chariot.def_source(base, patches, prepare) end

--- Define a package and return a reference to it.
--- @param pkg { platform: "host"|"target", name: string, version: string, revision: number, dependencies: Dependency[], runtime_dependencies: PackageRef[], configure?: string, build?: string, install: string }
--- @return PackageRef
function chariot.def_package(pkg) end
