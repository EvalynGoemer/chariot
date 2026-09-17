--- Check whether a string starts with a given substring.
--- @param s string
--- @param start string
function string.starts_with(s, start)
    return s:sub(1, #start) == start
end

--- Check whether a string ends with a given substring.
--- @param s string
--- @param ending string
function string.ends_with(s, ending)
    return ending == "" or s:sub(- #ending) == ending
end

--- Split a string by separator.
--- @param str string
--- @param separator string
--- @return string[]
function string.split(str, separator)
    separator = separator or "%s"

    local t = {}
    for str in string.gmatch(str, "([^" .. separator .. "]+)") do
        table.insert(t, str)
    end

    return t
end

--- Print table keys and values.
--- @param table table
function table.print(table)
    for k, v in pairs(table) do
        print(k .. "=" .. v)
    end
end
